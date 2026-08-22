//! 一次性工具：生成 argon2id PHC 密码哈希（用于回归测试临时重置账号密码）。
//! 用法: cargo run --release --example genpw -- <明文密码>

fn main() {
    let plain = std::env::args().nth(1).expect("需要一个明文密码参数");
    let hash = cortex_agent::domain::auth::password::hash_password(&plain).expect("哈希失败");
    println!("{hash}");
}
