# rust qqbot
一个由于rust编写的机器人支持随机消息推送

#### 交叉编译
.cargo/config.toml里面必须要进行配置
windows
~~~cmd
cargo build --release --target x86_64-pc-windows-gnu   
~~~~

linux

~~~cmd
cargo build --release --target x86_64-unknown-linux-musl   
~~~