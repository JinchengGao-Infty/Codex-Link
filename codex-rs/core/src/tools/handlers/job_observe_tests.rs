use super::read_log_range;
use super::read_log_tail;
use super::truncate_command;
use pretty_assertions::assert_eq;

#[test]
fn tail_returns_last_lines_and_total_size() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("exec-1.log");
    let content: String = (1..=100).map(|i| format!("line {i}\n")).collect();
    std::fs::write(&path, &content).expect("write log");

    let (tail, total) = read_log_tail(&path, 3).expect("tail should read");
    assert_eq!(tail, "line 98\nline 99\nline 100");
    assert_eq!(total, content.len() as u64);
}

#[test]
fn read_range_is_bounded_and_reports_total() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("exec-1.log");
    std::fs::write(&path, "0123456789").expect("write log");

    let (chunk, total) = read_log_range(&path, 2, 4).expect("range should read");
    assert_eq!(chunk, "2345");
    assert_eq!(total, 10);

    let (past_end, _) = read_log_range(&path, 50, 4).expect("offset past end should succeed");
    assert_eq!(past_end, "");
}

#[test]
fn missing_log_reads_return_none() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("absent.log");
    assert!(read_log_tail(&path, 5).is_none());
    assert!(read_log_range(&path, 0, 5).is_none());
}

#[test]
fn long_commands_are_truncated_for_display() {
    let long = "x".repeat(500);
    let rendered = truncate_command(&long);
    assert_eq!(rendered.chars().count(), 121);
    assert!(rendered.ends_with('…'));
    assert_eq!(truncate_command("short"), "short");
}
