//! 会话模块体外单元测试（对齐 grep_tests.rs 先例）。

#[test]
fn session_id_generation_is_uuid_v7() {
    let id = uuid::Uuid::now_v7().to_string();
    let parsed = uuid::Uuid::parse_str(&id).unwrap();
    assert_eq!(parsed.get_version_num(), 7);
    // Check RFC 4122 variant
    let bytes = parsed.as_bytes();
    let variant_byte = bytes[8];
    assert!(
        (variant_byte & 0b11000000 == 0b10000000) || (variant_byte & 0b11000000 == 0b11000000),
        "must be RFC4122 variant (bit 6 set, or 7 set for legacy)"
    );
}

#[test]
fn uuid_v7_millis_extracts_timestamp() {
    let id = uuid::Uuid::now_v7().to_string();
    let ms = super::uuid_v7_millis(&id).expect("v7 id 应能解出毫秒");
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    // 解出的创建毫秒应贴近当前时间（容差 1 分钟）
    assert!(
        ms.abs_diff(now_ms) < 60_000,
        "解出的毫秒 {ms} 与当前 {now_ms} 相差过大"
    );
}

#[test]
fn uuid_v7_millis_rejects_non_v7() {
    // v4 id 无时间戳，应返回 None
    let v4 = uuid::Uuid::new_v4().to_string();
    assert!(super::uuid_v7_millis(&v4).is_none());
    // 非法字符串
    assert!(super::uuid_v7_millis("not-a-uuid").is_none());
}

#[test]
fn uuid_v7_string_descending_is_creation_descending() {
    // 连续生成两个 v7 id（后者更新），字符串倒序应把更新的排前面
    let older = uuid::Uuid::now_v7().to_string();
    // 确保跨过毫秒边界，避免同毫秒随机位影响
    std::thread::sleep(std::time::Duration::from_millis(2));
    let newer = uuid::Uuid::now_v7().to_string();
    let mut ids = [older.clone(), newer.clone()];
    ids.sort_by(|a, b| b.cmp(a)); // 与列表排序同逻辑
    assert_eq!(ids[0], newer, "倒序后最新创建的应排第一");
    assert_eq!(ids[1], older);
}
