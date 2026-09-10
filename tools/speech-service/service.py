#!/usr/bin/env python3
"""芸汐的本机语音服务：中文语音识别（ASR）+ 中文语音合成（TTS）。

只监听回环地址，只被同一台机器上的 kovi-bot 调用。协议：

    GET  /healthz      -> {"ok": true, "asr": ..., "tts": ..., "tts_sample_rate": ...}
    POST /v1/asr       <- audio/wav（单声道 16 位 PCM）
                       -> {"text": "..."}
    POST /v1/tts       <- {"text": "...", "sample_rate": 24000}
                       -> 单声道 S16LE 裸 PCM，响应头 X-Sample-Rate 给出实际采样率

设计取舍：

* 用 sherpa-onnx 的预编译 wheel，服务器上 ``pip install`` 即可，不需要把
  ONNX Runtime 链接进机器人二进制，CI 构建保持原样。
* 识别与合成各持一把锁、用两个独立模型实例，因此"芸汐正在说话"不会阻塞
  下一句语音的识别；ONNX Runtime 在推理时会释放 GIL，线程模型足够。
* 合成按句切分、逐句生成、逐句下发：第一句合成完就开始播，而不是等整段。
  sherpa-onnx 没有真正的流式 TTS 接口，逐句流水线是它最接近的形态。
* 模型文件从目录里自动发现（tokens / lexicon / dict / *.fst / vocoder），
  避免把十几个路径写进 systemd 单元后错一个就跑不起来。
"""

from __future__ import annotations

import argparse
import io
import json
import logging
import re
import sys
import threading
import wave
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

try:
    import numpy as np
except ImportError:  # pragma: no cover - 部署前由 requirements.txt 保证
    print("缺少 numpy：请先运行 pip install -r requirements.txt", file=sys.stderr)
    raise SystemExit(1)

LOGGER = logging.getLogger("speech-service")

# 单个请求体上限。一小时 16 kHz 单声道 S16LE 也只有 115 MB，这里取 16 MB，
# 足够 8 分钟连续语音，同时挡住畸形请求。
MAX_BODY_BYTES = 16 * 1024 * 1024
# 合成请求的最大字数。
MAX_TTS_CHARS = 500
# 允许监听的主机：只接受回环。
LOOPBACK_HOSTS = {"127.0.0.1", "::1", "localhost"}

# 按中文句末标点切句，让第一句尽早出声。
SENTENCE_SPLIT = re.compile(r"(?<=[。！？!?…；;\n])")


# --- 模型发现 ---------------------------------------------------------------


class ModelFiles:
    """从模型目录里发现 sherpa-onnx 需要的文件。"""

    @staticmethod
    def asr(directory: str) -> dict:
        root = Path(directory).expanduser()
        if not root.is_dir():
            raise FileNotFoundError(f"识别模型目录不存在: {root}")
        model = ModelFiles._first(root, ("model.int8.onnx", "model.onnx", "model.onnx.int8"))
        if model is None:
            raise FileNotFoundError(f"{root} 下没有 model.onnx 或 model.int8.onnx")
        tokens = root / "tokens.txt"
        if not tokens.is_file():
            raise FileNotFoundError(f"{root} 下没有 tokens.txt")
        return {"model": str(model), "tokens": str(tokens), "dir": str(root)}

    @staticmethod
    def tts(directory: str, kind: str) -> dict:
        root = Path(directory).expanduser()
        if not root.is_dir():
            raise FileNotFoundError(f"合成模型目录不存在: {root}")
        files: dict = {"dir": str(root), "kind": kind}

        if kind == "matcha":
            files["acoustic_model"] = str(
                ModelFiles._require(root, ("model.onnx",), "matcha 声学模型 model.onnx")
            )
            vocoder = ModelFiles._first(
                root, ("vocoder.onnx", "vocos-22khz-univ.onnx", "hifigan_v2.onnx")
            )
            if vocoder is None:
                raise FileNotFoundError(
                    f"{root} 下没有 vocoder.onnx；matcha 需要额外的声码器文件"
                )
            files["vocoder"] = str(vocoder)
        else:
            files["model"] = str(
                ModelFiles._require(root, ("model.onnx", "model.int8.onnx"), "VITS 模型")
            )

        files["tokens"] = str(ModelFiles._require(root, ("tokens.txt",), "tokens.txt"))
        files["lexicon"] = str(ModelFiles._require(root, ("lexicon.txt",), "lexicon.txt"))

        dict_dir = root / "dict"
        # 只有真的存在且有内容才传：空目录会让 sherpa-onnx 的分词器初始化失败。
        has_dict = dict_dir.is_dir() and any(dict_dir.iterdir())
        files["dict_dir"] = str(dict_dir) if has_dict else ""
        # phone/date/number/new_heteronym 等文本正则：数字、日期、多音字靠它们
        # 才读得对，缺了会逐字念。
        files["rule_fsts"] = ",".join(sorted(str(path) for path in root.glob("*.fst")))
        return files

    @staticmethod
    def _first(root: Path, names: tuple[str, ...]) -> Path | None:
        for name in names:
            candidate = root / name
            if candidate.is_file():
                return candidate
        return None

    @staticmethod
    def _require(root: Path, names: tuple[str, ...], label: str) -> Path:
        found = ModelFiles._first(root, names)
        if found is None:
            raise FileNotFoundError(f"{root} 下缺少 {label}")
        return found


# --- 引擎 -------------------------------------------------------------------


class SpeechEngine:
    """懒加载、可并发使用的识别与合成引擎。"""

    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self._asr = None
        self._tts = None
        self._asr_lock = threading.Lock()
        self._tts_lock = threading.Lock()
        self._load_error: str | None = None
        self._asr_files: dict = {}
        self._tts_files: dict = {}

    def load(self) -> None:
        import sherpa_onnx

        self._asr_files = ModelFiles.asr(self.args.asr_dir)
        LOGGER.info("加载识别模型: %s", self._asr_files["model"])
        self._asr = sherpa_onnx.OfflineRecognizer.from_sense_voice(
            model=self._asr_files["model"],
            tokens=self._asr_files["tokens"],
            num_threads=self.args.threads,
            use_itn=True,
            language=self.args.asr_language,
            debug=False,
        )

        self._tts_files = ModelFiles.tts(self.args.tts_dir, self.args.tts_kind)
        LOGGER.info(
            "加载合成模型: %s（kind=%s）", self._tts_files["dir"], self.args.tts_kind
        )
        common = {
            "tokens": self._tts_files["tokens"],
            "lexicon": self._tts_files["lexicon"],
            "dict_dir": self._tts_files["dict_dir"] or None,
        }
        if self.args.tts_kind == "matcha":
            model = sherpa_onnx.OfflineTtsModelConfig(
                matcha=sherpa_onnx.OfflineTtsMatchaModelConfig(
                    acoustic_model=self._tts_files["acoustic_model"],
                    vocoder=self._tts_files["vocoder"],
                    **common,
                ),
                num_threads=self.args.threads,
                provider="cpu",
            )
        else:
            model = sherpa_onnx.OfflineTtsModelConfig(
                vits=sherpa_onnx.OfflineTtsVitsModelConfig(
                    model=self._tts_files["model"], **common
                ),
                num_threads=self.args.threads,
                provider="cpu",
            )
        self._tts = sherpa_onnx.OfflineTts(
            sherpa_onnx.OfflineTtsConfig(
                model=model,
                # 1 表示一次处理一句；配合下面的逐句循环，就是逐句下发。
                max_num_sentences=1,
                rule_fsts=self._tts_files["rule_fsts"] or "",
            )
        )
        LOGGER.info(
            "模型就绪：识别 %s，合成 %s Hz",
            Path(self._asr_files["dir"]).name,
            self._tts.sample_rate,
        )

    @property
    def ready(self) -> bool:
        return self._asr is not None and self._tts is not None

    @property
    def sample_rate(self) -> int:
        return int(self._tts.sample_rate) if self._tts is not None else 0

    @property
    def load_error(self) -> str | None:
        return self._load_error

    def remember_error(self, error: BaseException) -> None:
        self._load_error = str(error)

    def transcribe(self, wav_bytes: bytes) -> str:
        with wave.open(io.BytesIO(wav_bytes), "rb") as handle:
            channels = handle.getnchannels()
            width = handle.getsampwidth()
            rate = handle.getframerate()
            frames = handle.readframes(handle.getnframes())
        if width != 2:
            raise ValueError(f"只支持 16 位 PCM，收到 {width * 8} 位")
        if channels != 1:
            raise ValueError(f"只支持单声道，收到 {channels} 声道")
        samples = np.frombuffer(frames, dtype=np.int16).astype(np.float32) / 32768.0
        if samples.size == 0:
            return ""
        with self._asr_lock:
            stream = self._asr.create_stream()
            stream.accept_waveform(rate, samples)
            self._asr.decode_stream(stream)
            return str(stream.result.text).strip()

    def synthesize(self, text: str, target_rate: int | None):
        """按句合成，逐句 yield ``(采样率, int16 PCM 字节)``。"""
        for sentence in split_sentences(text):
            with self._tts_lock:
                audio = self._tts.generate(
                    sentence, sid=self.args.tts_speaker, speed=self.args.tts_speed
                )
            samples = np.asarray(audio.samples, dtype=np.float32)
            if samples.size == 0:
                continue
            rate = int(audio.sample_rate)
            if target_rate and rate != target_rate:
                samples = resample(samples, rate, target_rate)
                rate = target_rate
            pcm = np.clip(samples * 32767.0, -32768.0, 32767.0).astype(np.int16)
            yield rate, pcm.tobytes()


def split_sentences(text: str) -> list[str]:
    """把一段话切成可独立合成的句子；切不动时整段返回。"""
    parts = [part.strip() for part in SENTENCE_SPLIT.split(text) if part.strip()]
    return parts or ([text.strip()] if text.strip() else [])


def resample(samples: "np.ndarray", source_rate: int, target_rate: int) -> "np.ndarray":
    """线性插值重采样。

    只有在调用方明确指定了和模型不同的采样率时才会走到这里；常规路径是
    让 PulseAudio 在自己的 sink 上重采样。通话链路对音质要求不高，线性插值
    够用且不引入额外依赖。
    """
    if source_rate == target_rate or samples.size == 0:
        return samples
    duration = samples.size / float(source_rate)
    target_count = max(1, int(round(duration * target_rate)))
    source_positions = np.linspace(0.0, duration, num=samples.size, endpoint=False)
    target_positions = np.linspace(0.0, duration, num=target_count, endpoint=False)
    return np.interp(target_positions, source_positions, samples).astype(np.float32)


# --- HTTP -------------------------------------------------------------------


class Handler(BaseHTTPRequestHandler):
    server_version = "YunxiSpeech/1.0"
    # 流式响应没有 Content-Length，用 HTTP/1.0 的"连接关闭即结束"语义。
    protocol_version = "HTTP/1.0"

    engine: SpeechEngine

    def log_message(self, fmt: str, *args) -> None:  # noqa: A003 - 基类签名
        LOGGER.debug("%s - %s", self.address_string(), fmt % args)

    def _send_json(self, status: HTTPStatus, payload: dict) -> None:
        body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def _read_body(self) -> bytes | None:
        try:
            length = int(self.headers.get("Content-Length", "0"))
        except ValueError:
            self._send_json(HTTPStatus.BAD_REQUEST, {"error": "非法的 Content-Length"})
            return None
        if length <= 0:
            self._send_json(HTTPStatus.BAD_REQUEST, {"error": "请求体为空"})
            return None
        if length > MAX_BODY_BYTES:
            self._send_json(HTTPStatus.REQUEST_ENTITY_TOO_LARGE, {"error": "请求体过大"})
            return None
        return self.rfile.read(length)

    def do_GET(self) -> None:  # noqa: N802 - 基类签名
        if self.path.split("?")[0] != "/healthz":
            self._send_json(HTTPStatus.NOT_FOUND, {"error": "not found"})
            return
        ready = self.engine.ready
        payload = {
            "ok": ready,
            "asr": self.engine.ready,
            "tts": self.engine.ready,
            "tts_sample_rate": self.engine.sample_rate,
        }
        if not ready and self.engine.load_error:
            payload["error"] = self.engine.load_error
        self._send_json(HTTPStatus.OK if ready else HTTPStatus.SERVICE_UNAVAILABLE, payload)

    def do_POST(self) -> None:  # noqa: N802 - 基类签名
        path = self.path.split("?")[0]
        if path == "/v1/asr":
            self._handle_asr()
        elif path == "/v1/tts":
            self._handle_tts()
        else:
            self._send_json(HTTPStatus.NOT_FOUND, {"error": "not found"})

    def _handle_asr(self) -> None:
        if not self.engine.ready:
            self._send_json(HTTPStatus.SERVICE_UNAVAILABLE, {"error": "模型尚未就绪"})
            return
        body = self._read_body()
        if body is None:
            return
        try:
            text = self.engine.transcribe(body)
        except Exception as error:  # noqa: BLE001 - 单次请求失败不应影响服务
            LOGGER.warning("识别失败: %s", error)
            self._send_json(HTTPStatus.BAD_REQUEST, {"error": f"识别失败: {error}"})
            return
        LOGGER.info("识别: %s", text)
        self._send_json(HTTPStatus.OK, {"text": text})

    def _handle_tts(self) -> None:
        if not self.engine.ready:
            self._send_json(HTTPStatus.SERVICE_UNAVAILABLE, {"error": "模型尚未就绪"})
            return
        body = self._read_body()
        if body is None:
            return
        try:
            payload = json.loads(body.decode("utf-8"))
        except Exception:  # noqa: BLE001
            self._send_json(HTTPStatus.BAD_REQUEST, {"error": "请求体不是合法 JSON"})
            return
        text = str(payload.get("text", "")).strip()
        if not text:
            self._send_json(HTTPStatus.BAD_REQUEST, {"error": "text 不能为空"})
            return
        if len(text) > MAX_TTS_CHARS:
            self._send_json(HTTPStatus.BAD_REQUEST, {"error": "text 过长"})
            return
        requested = payload.get("sample_rate")
        requested = int(requested) if isinstance(requested, int) and requested > 0 else None

        responded = False
        try:
            for rate, pcm in self.engine.synthesize(text, requested):
                if not responded:
                    # 采样率要等第一块音频才知道，所以先拿到它再发响应头。
                    self.send_response(HTTPStatus.OK)
                    self.send_header("Content-Type", "audio/L16")
                    self.send_header("X-Sample-Rate", str(rate))
                    self.send_header("Cache-Control", "no-store")
                    self.end_headers()
                    responded = True
                self.wfile.write(pcm)
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            # 对方插话时 bot 会直接断开播放流，这是正常路径。
            LOGGER.debug("合成流被客户端关闭")
            return
        except Exception as error:  # noqa: BLE001 - 单次请求失败不应影响服务
            LOGGER.warning("合成失败: %s", error)
            if not responded:
                self._send_json(
                    HTTPStatus.INTERNAL_SERVER_ERROR, {"error": f"合成失败: {error}"}
                )
            return
        if not responded:
            # 一句话都没合成出来（例如全是标点）。
            self.send_response(HTTPStatus.OK)
            self.send_header("Content-Type", "audio/L16")
            self.send_header("X-Sample-Rate", str(self.engine.sample_rate))
            self.send_header("Content-Length", "0")
            self.end_headers()


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="芸汐本机语音服务（ASR + TTS）")
    parser.add_argument("--host", default="127.0.0.1", help="监听地址；只允许回环")
    parser.add_argument("--port", type=int, default=6120)
    parser.add_argument(
        "--threads",
        type=int,
        default=2,
        help="每个模型的 ONNX 线程数；4 核机器建议 2，给 QQ 和系统留余量",
    )

    parser.add_argument(
        "--asr-dir", required=True, help="识别模型目录（含 model*.onnx 与 tokens.txt）"
    )
    parser.add_argument(
        "--asr-language", default="zh", help="识别语言提示：zh/en/ja/ko/yue/auto"
    )

    parser.add_argument(
        "--tts-dir", required=True, help="合成模型目录（含 model.onnx、tokens.txt、lexicon.txt）"
    )
    parser.add_argument(
        "--tts-kind",
        default="vits",
        choices=("vits", "matcha"),
        help="合成模型类型；matcha 需要目录里同时有声码器",
    )
    parser.add_argument("--tts-speaker", type=int, default=0, help="音色编号")
    parser.add_argument("--tts-speed", type=float, default=1.0, help="语速倍率")

    parser.add_argument("--log-level", default="INFO")
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    logging.basicConfig(
        level=getattr(logging, args.log_level.upper(), logging.INFO),
        format="%(asctime)s %(levelname)s %(message)s",
    )
    if args.host not in LOOPBACK_HOSTS:
        LOGGER.error("只允许监听回环地址，收到 %s", args.host)
        return 2

    engine = SpeechEngine(args)
    try:
        engine.load()
    except Exception as error:  # noqa: BLE001 - 启动期错误必须留下线索
        # 模型加载失败也把 HTTP 服务拉起来，让 /healthz 报出原因，
        # 而不是让 systemd 反复重启却不留线索。
        engine.remember_error(error)
        LOGGER.error("模型加载失败，服务以未就绪状态运行: %s", error)

    Handler.engine = engine
    server = ThreadingHTTPServer((args.host, args.port), Handler)
    server.daemon_threads = True
    LOGGER.info("语音服务监听 http://%s:%s", args.host, args.port)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        LOGGER.info("收到中断，正在退出")
    finally:
        server.server_close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
