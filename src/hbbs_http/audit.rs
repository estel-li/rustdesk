//! 统一审计上报管道。
//!
//! 目标：
//! 1. 所有审计上报 fire-and-forget，不阻塞远控会话；
//! 2. 有界队列 + 队列满时丢最旧条目；
//! 3. 失败重试有限次数 + 退避。
//!
//! 见 docs/ai-tasks/CE-M1-7.md。

use hbb_common::{
    config::Config,
    lazy_static, log,
    tokio::{
        self,
        sync::Notify,
        time::{sleep, Duration},
    },
};
use serde_json::Value;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
};

/// 队列容量上限。超过该容量后，新条目会挤掉最旧的一条。
pub const AUDIT_QUEUE_CAPACITY: usize = 1000;
/// 单条审计的最大上送尝试次数（含首次）。
pub const AUDIT_RETRY_MAX: u32 = 3;
/// 失败后的退避序列（毫秒）。索引 i 表示第 i+1 次失败后等待的时长。
pub const AUDIT_RETRY_BACKOFF_MS: [u64; 3] = [500, 2000, 5000];
/// 剪贴板文本预览的最大字节数（UTF-8 安全截断）。
pub const CLIPBOARD_TEXT_PREVIEW_BYTES: usize = 64;

/// `typ` 段在 `/api/audit/{typ}` 中的取值。
pub const AUDIT_TYP_CONN: &str = "conn";
pub const AUDIT_TYP_FILE: &str = "file";
pub const AUDIT_TYP_ALARM: &str = "alarm";
pub const AUDIT_TYP_CLIPBOARD: &str = "clipboard";

/// `audit::enqueue` 失败时的原因，用于测试与日志诊断。
#[derive(Debug, PartialEq, Eq)]
pub enum DropReason {
    /// URL 为空 → 未配置 api-server，直接忽略。
    EmptyUrl,
    /// `audit-disable` 开关已开启，整个上报通道停摆。
    Disabled,
}

/// 队列消息形态。
#[derive(Debug, Clone)]
pub struct AuditJob {
    pub url: String,
    pub body: Value,
}

type AuditSink = Arc<dyn Fn(&AuditJob) -> bool + Send + Sync>;

struct AuditChannel {
    queue: Mutex<VecDeque<AuditJob>>,
    notify: Notify,
    capacity: usize,
}

impl AuditChannel {
    fn new(capacity: usize) -> Self {
        Self {
            queue: Mutex::new(VecDeque::with_capacity(capacity)),
            notify: Notify::new(),
            capacity,
        }
    }

    /// 入队；返回 true 表示出现了丢最旧的情况。
    fn push(&self, job: AuditJob) -> bool {
        let mut q = self.queue.lock().unwrap();
        let dropped_oldest = if q.len() >= self.capacity {
            q.pop_front();
            true
        } else {
            false
        };
        q.push_back(job);
        drop(q);
        self.notify.notify_one();
        dropped_oldest
    }

    fn pop(&self) -> Option<AuditJob> {
        self.queue.lock().unwrap().pop_front()
    }

    fn len(&self) -> usize {
        self.queue.lock().unwrap().len()
    }
}

lazy_static::lazy_static! {
    static ref CHANNEL: Arc<AuditChannel> = Arc::new(AuditChannel::new(AUDIT_QUEUE_CAPACITY));
    /// 通过测试钩子注入的 HTTP 上报函数。
    static ref TEST_SINK: Mutex<Option<AuditSink>> = Mutex::new(None);
}

/// 已被丢弃的最旧条目数（用于日志与可观测）。
static DROP_OLDEST_COUNT: AtomicU64 = AtomicU64::new(0);
/// worker 已启动标记。
static WORKER_STARTED: AtomicBool = AtomicBool::new(false);

/// 是否完全禁用审计上报（保险开关）。
pub fn is_disabled() -> bool {
    Config::get_option("audit-disable") == "Y"
}

/// 是否禁用剪贴板审计。
pub fn is_clipboard_disabled() -> bool {
    Config::get_option("audit-clipboard-disable") == "Y"
}

/// 长 relay 阈值（秒），未配置时默认 300。
pub fn long_relay_threshold_secs() -> u64 {
    let raw = Config::get_option("audit-long-relay-secs");
    raw.parse::<u64>().ok().filter(|v| *v > 0).unwrap_or(300)
}

/// 入队一条审计消息。
///
/// 调用方约定：
/// - 同步调用，永不阻塞、永不 await；
/// - 返回 `Ok(())` 不代表已上送成功，仅代表已进入队列；
/// - 队列满时会丢弃最旧的一条并把新的塞进去，依然返回 `Ok(())`，但会写一条 warn 日志；
/// - URL 为空（未配置 api-server）或 audit-disable=Y 时直接返回对应错误，不入队。
pub fn enqueue(url: String, body: Value) -> Result<(), DropReason> {
    if url.is_empty() {
        return Err(DropReason::EmptyUrl);
    }
    if is_disabled() {
        return Err(DropReason::Disabled);
    }

    ensure_worker_started();

    let dropped = CHANNEL.push(AuditJob { url, body });
    if dropped {
        let c = DROP_OLDEST_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        log::warn!(
            "audit queue full ({} cap), oldest dropped, total_dropped={}",
            AUDIT_QUEUE_CAPACITY,
            c
        );
    }
    Ok(())
}

/// 测试可见的丢最旧计数。
pub fn drop_oldest_count() -> u64 {
    DROP_OLDEST_COUNT.load(Ordering::Relaxed)
}

/// 测试可见的当前队列长度。
pub fn queue_len() -> usize {
    CHANNEL.len()
}

fn ensure_worker_started() {
    if WORKER_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let ch = CHANNEL.clone();
    tokio::spawn(async move {
        audit_worker(ch).await;
    });
}

async fn audit_worker(ch: Arc<AuditChannel>) {
    log::info!("audit worker started, capacity={}", AUDIT_QUEUE_CAPACITY);
    loop {
        let job = match ch.pop() {
            Some(j) => j,
            None => {
                ch.notify.notified().await;
                continue;
            }
        };
        run_job(&job).await;
    }
}

async fn run_job(job: &AuditJob) {
    let body = job.body.to_string();
    for attempt in 1..=AUDIT_RETRY_MAX {
        let ok = try_send_once(&job.url, &body).await;
        if ok {
            return;
        }
        if attempt < AUDIT_RETRY_MAX {
            let idx = (attempt as usize - 1).min(AUDIT_RETRY_BACKOFF_MS.len() - 1);
            sleep(Duration::from_millis(AUDIT_RETRY_BACKOFF_MS[idx])).await;
        } else {
            log::warn!(
                "audit upload give up after {} attempts, url={}",
                AUDIT_RETRY_MAX,
                tail_url(&job.url)
            );
        }
    }
}

async fn try_send_once(url: &str, body: &str) -> bool {
    // 测试钩子：直接走假投递。
    if let Some(hook) = TEST_SINK.lock().unwrap().clone() {
        let fake = AuditJob {
            url: url.to_owned(),
            body: serde_json::from_str(body).unwrap_or(Value::Null),
        };
        return hook(&fake);
    }
    match crate::post_request(url.to_owned(), body.to_owned(), "").await {
        Ok(_) => true,
        Err(e) => {
            log::debug!("audit upload failed url={}, err={}", tail_url(url), e);
            false
        }
    }
}

/// 仅保留 URL 末段用于日志，避免泄漏完整 API host。
fn tail_url(url: &str) -> &str {
    url.rsplit('/').next().unwrap_or(url)
}

/// 计算剪贴板内容的 hash + length + 可选 preview。
///
/// - `sha256`：完整 SHA-256 的前 32 字符（即前 16 字节）hex 表示；
/// - `length`：原始字节数；
/// - `preview`：仅当 `is_text=true` 时填充，UTF-8 安全截断到 ≤ `CLIPBOARD_TEXT_PREVIEW_BYTES` 字节。
pub fn hash_clipboard(content: &[u8], is_text: bool) -> (String, usize, Option<String>) {
    use hbb_common::sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(content);
    let digest = hasher.finalize();
    let hex: String = digest.iter().take(16).map(|b| format!("{:02x}", b)).collect();

    let preview = if is_text {
        Some(safe_utf8_prefix(content, CLIPBOARD_TEXT_PREVIEW_BYTES))
    } else {
        None
    };

    (hex, content.len(), preview)
}

/// 不劈半字符地把 `bytes` 截断为合法 UTF-8 字符串。
///
/// 实现思路：以 `max_bytes` 为上限，回退到最近的字符边界，再用 `from_utf8` 转回 String。
/// `max_bytes` 大于等于内容长度时直接返回完整内容（若整段 UTF-8 合法）。
pub fn safe_utf8_prefix(bytes: &[u8], max_bytes: usize) -> String {
    let end = bytes.len().min(max_bytes);
    let slice = &bytes[..end];
    match std::str::from_utf8(slice) {
        Ok(s) => s.to_owned(),
        Err(e) => {
            let valid = e.valid_up_to();
            // valid_up_to 必然是字符边界。
            String::from_utf8_lossy(&slice[..valid]).into_owned()
        }
    }
}

// -------- 测试钩子 --------

#[cfg(test)]
pub fn _test_set_sink<F>(f: F)
where
    F: Fn(&AuditJob) -> bool + Send + Sync + 'static,
{
    *TEST_SINK.lock().unwrap() = Some(Arc::new(f));
}

#[cfg(test)]
pub fn _test_clear_sink() {
    *TEST_SINK.lock().unwrap() = None;
}

#[cfg(test)]
pub fn _test_reset_counters() {
    DROP_OLDEST_COUNT.store(0, Ordering::Relaxed);
}

/// 仅供测试：构造一个独立 channel，便于单测验证丢最旧语义而不受全局状态影响。
#[cfg(test)]
pub fn _test_make_channel() -> Arc<AuditChannel> {
    Arc::new(AuditChannel::new(AUDIT_QUEUE_CAPACITY))
}

#[cfg(test)]
impl AuditChannel {
    pub fn _test_push(&self, job: AuditJob) -> bool {
        self.push(job)
    }
    pub fn _test_pop(&self) -> Option<AuditJob> {
        self.pop()
    }
    pub fn _test_len(&self) -> usize {
        self.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn enqueue_noop_when_url_empty() {
        let r = enqueue(String::new(), json!({"hello": "world"}));
        assert_eq!(r, Err(DropReason::EmptyUrl));
    }

    #[test]
    fn safe_utf8_prefix_does_not_split_char() {
        // "你好" 在 UTF-8 下是 6 字节，每个汉字 3 字节。
        let s = "你好abc";
        let bytes = s.as_bytes();
        let p = safe_utf8_prefix(bytes, 4);
        // 4 字节会切到 "你" + 第一字节 'b'，必须回退到 "你"（3 字节）。
        assert_eq!(p, "你");
        let p2 = safe_utf8_prefix(bytes, 100);
        assert_eq!(p2, "你好abc");
    }

    #[test]
    fn hash_clipboard_text_is_hashed_and_truncated() {
        let content = vec![b'a'; 5120];
        let (hex, len, preview) = hash_clipboard(&content, true);
        assert_eq!(len, 5120);
        assert_eq!(hex.len(), 32);
        let preview = preview.expect("text format should produce preview");
        assert!(preview.len() <= CLIPBOARD_TEXT_PREVIEW_BYTES);
        assert!(preview.chars().all(|c| c == 'a'));
    }

    #[test]
    fn hash_clipboard_image_omits_preview() {
        let content = vec![0u8; 1024 * 1024];
        let (hex, len, preview) = hash_clipboard(&content, false);
        assert_eq!(hex.len(), 32);
        assert_eq!(len, 1024 * 1024);
        assert!(preview.is_none());
    }

    #[test]
    fn channel_drops_oldest_when_full() {
        let ch = _test_make_channel();
        // 入队 capacity 条不丢。
        for i in 0..AUDIT_QUEUE_CAPACITY {
            let dropped = ch._test_push(AuditJob {
                url: "http://x".into(),
                body: json!({"seq": i}),
            });
            assert!(!dropped, "should not drop within capacity");
        }
        assert_eq!(ch._test_len(), AUDIT_QUEUE_CAPACITY);
        // 第 capacity+1 条应触发丢最旧。
        let dropped = ch._test_push(AuditJob {
            url: "http://x".into(),
            body: json!({"seq": AUDIT_QUEUE_CAPACITY}),
        });
        assert!(dropped, "should drop oldest at capacity+1");
        assert_eq!(ch._test_len(), AUDIT_QUEUE_CAPACITY);
        // 队首应该是 seq=1（seq=0 已被丢弃）。
        let first = ch._test_pop().unwrap();
        assert_eq!(first.body["seq"], json!(1u64));
        // 末尾应该是 seq=AUDIT_QUEUE_CAPACITY。
        let mut last = None;
        while let Some(j) = ch._test_pop() {
            last = Some(j);
        }
        assert_eq!(last.unwrap().body["seq"], json!(AUDIT_QUEUE_CAPACITY as u64));
    }

    #[hbb_common::tokio::test(flavor = "current_thread")]
    async fn worker_retries_then_gives_up() {
        use std::sync::atomic::{AtomicU32, Ordering};
        _test_reset_counters();
        let attempts = Arc::new(AtomicU32::new(0));
        let a = attempts.clone();
        _test_set_sink(move |_job| {
            a.fetch_add(1, Ordering::SeqCst);
            false // 总是失败
        });
        // 直接驱动 run_job，避开全局 worker 与 enqueue 的初始化竞争。
        // 注意：实际会触发两次 backoff sleep (500ms + 2000ms)，单测累计 ~2.5s。
        let job = AuditJob {
            url: "http://example/audit/conn".into(),
            body: json!({"x":1}),
        };
        run_job(&job).await;
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            AUDIT_RETRY_MAX,
            "should attempt exactly AUDIT_RETRY_MAX times"
        );
        _test_clear_sink();
    }

    #[hbb_common::tokio::test(flavor = "current_thread")]
    async fn worker_stops_on_first_success() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let attempts = Arc::new(AtomicU32::new(0));
        let a = attempts.clone();
        _test_set_sink(move |_job| {
            a.fetch_add(1, Ordering::SeqCst);
            true
        });
        let job = AuditJob {
            url: "http://example/audit/conn".into(),
            body: json!({"x":1}),
        };
        run_job(&job).await;
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        _test_clear_sink();
    }
}
