#!/usr/bin/env python3
"""芸汐的本机嵌入服务：把中文文本编码成向量，供记忆检索的语义那一路使用。

只监听回环地址，只被同一台机器上的 kovi-bot 调用。协议：

    GET  /healthz   -> {"ok": true, "model": ..., "dim": ..., "reranker": ..., "ready": true}
    POST /v1/embed  <- {"texts": ["...", "..."]}
                    -> {"vectors": [[...], [...]], "dim": 512, "model": "..."}
    POST /v1/rerank <- {"query": "...", "documents": ["..."], "top_n": 5}
                    -> {"results": [{"index": 0, "score": 0.87}], "model": "..."}
                       （重排器未安装时返回 503，调用方据此跳过重排）

设计取舍（照抄 yunxi-speech 那套，理由同样成立）：

* **独立进程**：只用 onnxruntime + tokenizers 的预编译 wheel，不把 ONNX Runtime
  链接进机器人二进制，CI 构建保持原样。
* **标准库 HTTP**：与语音服务一致，不引 FastAPI/uvicorn。这个服务只有两个端点。
* **BGE 的用法**：bge 系列是**非对称**检索模型，查询侧要加指令前缀
  （``为这个句子生成表示以用于检索相关文章：``），文档侧不加。搞反了会明显掉点，
  所以前缀在服务里定死，调用方不需要知道。
* **归一化**：输出做 L2 归一化，于是余弦相似度退化成点积，调用方不必再算模长。
* **批量**：一次请求编码多段文本，避免每段一次 HTTP 往返。
"""

from __future__ import annotations

import argparse
import json
import logging
import sys
import threading
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import numpy as np
import onnxruntime as ort
from tokenizers import Tokenizer

LOGGER = logging.getLogger("yunxi-embed")

# bge 系列查询侧的标准指令前缀。文档侧不加——这是模型卡里写明的用法。
QUERY_PREFIX = "为这个句子生成表示以用于检索相关文章："

# 单次请求最多编码多少段，防止一个坏请求把内存吃穿。
MAX_BATCH = 64
# 单段最大字符数（超长截断，bge 的窗口是 512 token）。
MAX_CHARS = 1024


class Embedder:
    """ONNX 会话 + 分词器。推理时释放 GIL，所以多线程 HTTP 足够。"""

    def __init__(self, model_path: Path, tokenizer_path: Path) -> None:
        self.session = ort.InferenceSession(
            str(model_path), providers=["CPUExecutionProvider"]
        )
        self.tokenizer = Tokenizer.from_file(str(tokenizer_path))
        self.tokenizer.enable_truncation(max_length=512)
        self.tokenizer.enable_padding()
        inputs = {item.name for item in self.session.get_inputs()}
        self.needs_token_types = "token_type_ids" in inputs
        self.dim = int(self.session.get_outputs()[0].shape[-1])

    def encode(self, texts: list[str], query: bool) -> list[list[float]]:
        prepared = [
            (QUERY_PREFIX + text) if query else text for text in texts
        ]
        encodings = self.tokenizer.encode_batch(prepared)
        input_ids = np.array([item.ids for item in encodings], dtype=np.int64)
        attention = np.array([item.attention_mask for item in encodings], dtype=np.int64)
        feed = {"input_ids": input_ids, "attention_mask": attention}
        if self.needs_token_types:
            feed["token_type_ids"] = np.array(
                [item.type_ids for item in encodings], dtype=np.int64
            )
        # bge 用 CLS 位置的池化。
        hidden = self.session.run(None, feed)[0][:, 0, :]
        norms = np.linalg.norm(hidden, axis=1, keepdims=True)
        norms[norms == 0] = 1.0
        normalized = hidden / norms
        return normalized.astype(np.float32).tolist()


class Reranker:
    """cross-encoder 重排器：把 (查询, 文档) 成对打分。

    与嵌入模型的区别：嵌入是"各自编码再算距离"，重排是**把两段文本一起送进模型**，
    所以更准也更慢——它只该用在"已经筛出来的少量候选"上，不该拿来做召回。
    """

    def __init__(self, model_path: Path, tokenizer_path: Path) -> None:
        self.session = ort.InferenceSession(
            str(model_path), providers=["CPUExecutionProvider"]
        )
        self.tokenizer = Tokenizer.from_file(str(tokenizer_path))
        self.tokenizer.enable_truncation(max_length=512)
        self.tokenizer.enable_padding()
        inputs = {item.name for item in self.session.get_inputs()}
        self.needs_token_types = "token_type_ids" in inputs

    def score(self, query: str, documents: list[str]) -> list[float]:
        pairs = [(query, document) for document in documents]
        encodings = self.tokenizer.encode_batch(pairs)
        input_ids = np.array([item.ids for item in encodings], dtype=np.int64)
        attention = np.array([item.attention_mask for item in encodings], dtype=np.int64)
        feed = {"input_ids": input_ids, "attention_mask": attention}
        if self.needs_token_types:
            feed["token_type_ids"] = np.array(
                [item.type_ids for item in encodings], dtype=np.int64
            )
        logits = self.session.run(None, feed)[0]
        flat = np.asarray(logits).reshape(-1)
        # bge 重排器用 sigmoid 把 logit 压到 0..1，便于当阈值用。
        return (1.0 / (1.0 + np.exp(-flat))).astype(np.float32).tolist()


class Handler(BaseHTTPRequestHandler):
    embedder: Embedder
    model_name: str
    reranker: Reranker | None = None
    reranker_name: str = ""

    def log_message(self, format: str, *args) -> None:  # noqa: A002
        LOGGER.info("%s - %s", self.address_string(), format % args)

    def _send(self, status: HTTPStatus, payload: dict) -> None:
        body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _handle_rerank(self) -> None:
        if self.reranker is None:
            self._send(
                HTTPStatus.SERVICE_UNAVAILABLE,
                {"ok": False, "error": "重排器未安装（启动时没给 --reranker-model）"},
            )
            return
        payload = self._read_json()
        if payload is None:
            return
        query = str(payload.get("query", "")).strip()
        documents = payload.get("documents")
        if not query or not isinstance(documents, list) or not documents:
            self._send(
                HTTPStatus.BAD_REQUEST,
                {"ok": False, "error": "query 与 documents 都必须非空"},
            )
            return
        if len(documents) > MAX_BATCH:
            self._send(
                HTTPStatus.BAD_REQUEST,
                {"ok": False, "error": f"一次最多 {MAX_BATCH} 篇"},
            )
            return
        cleaned = [str(document)[:MAX_CHARS] for document in documents]
        try:
            scores = self.reranker.score(query, cleaned)
        except Exception as error:  # noqa: BLE001
            LOGGER.exception("rerank failed")
            self._send(
                HTTPStatus.INTERNAL_SERVER_ERROR,
                {"ok": False, "error": f"重排失败: {error}"},
            )
            return
        ranked = sorted(
            ({"index": index, "score": score} for index, score in enumerate(scores)),
            key=lambda item: item["score"],
            reverse=True,
        )
        top_n = payload.get("top_n")
        if isinstance(top_n, int) and top_n > 0:
            ranked = ranked[:top_n]
        self._send(
            HTTPStatus.OK,
            {"results": ranked, "model": self.reranker_name},
        )

    def _read_json(self) -> dict | None:
        """读一个 JSON 请求体；出错时已经回过响应，返回 None。"""
        try:
            length = int(self.headers.get("Content-Length", "0"))
        except ValueError:
            self._send(HTTPStatus.BAD_REQUEST, {"ok": False, "error": "bad length"})
            return None
        if length <= 0 or length > 1_000_000:
            self._send(HTTPStatus.BAD_REQUEST, {"ok": False, "error": "bad body size"})
            return None
        try:
            payload = json.loads(self.rfile.read(length).decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            self._send(HTTPStatus.BAD_REQUEST, {"ok": False, "error": f"bad json: {error}"})
            return None
        if not isinstance(payload, dict):
            self._send(HTTPStatus.BAD_REQUEST, {"ok": False, "error": "body 必须是对象"})
            return None
        return payload

    def do_GET(self) -> None:  # noqa: N802
        if self.path != "/healthz":
            self._send(HTTPStatus.NOT_FOUND, {"ok": False, "error": "unknown path"})
            return
        self._send(
            HTTPStatus.OK,
            {
                "ok": True,
                "ready": True,
                "model": self.model_name,
                "dim": self.embedder.dim,
                "query_prefix": QUERY_PREFIX,
                "reranker": self.reranker_name or None,
            },
        )

    def do_POST(self) -> None:  # noqa: N802
        if self.path == "/v1/rerank":
            self._handle_rerank()
            return
        if self.path != "/v1/embed":
            self._send(HTTPStatus.NOT_FOUND, {"ok": False, "error": "unknown path"})
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
        except ValueError:
            self._send(HTTPStatus.BAD_REQUEST, {"ok": False, "error": "bad length"})
            return
        if length <= 0 or length > 1_000_000:
            self._send(HTTPStatus.BAD_REQUEST, {"ok": False, "error": "bad body size"})
            return
        try:
            payload = json.loads(self.rfile.read(length).decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            self._send(HTTPStatus.BAD_REQUEST, {"ok": False, "error": f"bad json: {error}"})
            return
        texts = payload.get("texts")
        if not isinstance(texts, list) or not texts:
            self._send(HTTPStatus.BAD_REQUEST, {"ok": False, "error": "texts 必须是非空数组"})
            return
        if len(texts) > MAX_BATCH:
            self._send(
                HTTPStatus.BAD_REQUEST,
                {"ok": False, "error": f"一次最多 {MAX_BATCH} 段"},
            )
            return
        cleaned = [str(text)[:MAX_CHARS] for text in texts]
        try:
            vectors = self.embedder.encode(cleaned, query=bool(payload.get("query", False)))
        except Exception as error:  # noqa: BLE001 - 出错要如实回给调用方
            LOGGER.exception("embedding failed")
            self._send(
                HTTPStatus.INTERNAL_SERVER_ERROR,
                {"ok": False, "error": f"编码失败: {error}"},
            )
            return
        self._send(
            HTTPStatus.OK,
            {"vectors": vectors, "dim": self.embedder.dim, "model": self.model_name},
        )


def main() -> int:
    parser = argparse.ArgumentParser(description="芸汐本机嵌入服务")
    parser.add_argument("--model", required=True, help="ONNX 模型路径")
    parser.add_argument("--tokenizer", required=True, help="tokenizer.json 路径")
    parser.add_argument("--model-name", default="bge-small-zh-v1.5")
    # 重排器可选：没给路径时 /v1/rerank 返回 503，调用方跳过重排即可，
    # 嵌入服务本身照常工作。
    parser.add_argument("--reranker-model", default="")
    parser.add_argument("--reranker-tokenizer", default="")
    parser.add_argument("--reranker-name", default="bge-reranker-base")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=6112)
    args = parser.parse_args()

    logging.basicConfig(
        level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s"
    )
    embedder = Embedder(Path(args.model), Path(args.tokenizer))
    # 预热：第一次推理要建会话与内存池，别让第一个真实请求承担这几秒。
    embedder.encode(["预热"], query=False)
    LOGGER.info("模型就绪：%s，维度 %d", args.model_name, embedder.dim)

    reranker = None
    if args.reranker_model and args.reranker_tokenizer:
        reranker = Reranker(Path(args.reranker_model), Path(args.reranker_tokenizer))
        # 预热：重排是每轮检索都要走的，别让第一个真实请求等模型初始化。
        reranker.score("预热", ["预热文档"])
        LOGGER.info("重排器就绪：%s", args.reranker_name)

    Handler.embedder = embedder
    Handler.model_name = args.model_name
    Handler.reranker = reranker
    Handler.reranker_name = args.reranker_name if reranker else ""
    server = ThreadingHTTPServer((args.host, args.port), Handler)
    LOGGER.info("监听 http://%s:%d", args.host, args.port)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
