#![cfg(feature = "integration-tests")]

use model::test_support::{
    accepts_public_image_url, redis_runtime_round_trip, reply_ticket_generation_is_atomic,
};

#[test]
fn ssrf_policy_rejects_private_and_redirect_prone_sources() {
    for url in [
        "http://127.0.0.1/image.png",
        "http://169.254.169.254/latest/meta-data",
        "http://[::1]/image.png",
        "https://user:password@example.com/image.png",
    ] {
        assert!(!accepts_public_image_url(url), "不应接受不安全来源: {url}");
    }
    assert!(accepts_public_image_url("https://example.com/image.png"));
}

#[test]
fn reply_ticket_generation_cannot_be_released_by_a_stale_task() {
    let runtime = kovi::tokio::runtime::Runtime::new().expect("应创建测试运行时");
    assert!(runtime.block_on(reply_ticket_generation_is_atomic()));
}

#[test]
#[ignore = "requires Redis via REDIS_URL"]
fn redis_runtime_store_round_trips_through_the_black_box_api() {
    let runtime = kovi::tokio::runtime::Runtime::new().expect("应创建测试运行时");
    runtime
        .block_on(redis_runtime_round_trip())
        .expect("Redis 集成往返应成功");
}
