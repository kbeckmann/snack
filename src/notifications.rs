use std::sync::{Mutex, mpsc::{self, SyncSender, TrySendError}};

const QUEUE_CAPACITY: usize = 32;
const SUMMARY_CHAR_LIMIT: usize = 128;
const BODY_CHAR_LIMIT: usize = 512;

struct Notice
{
    summary: String,
    body: String,
}

fn preview(text: &str, limit: usize) -> String
{
    let mut chars = text.chars();
    let mut preview: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() { preview.push('…'); }
    preview
}

impl Notice
{
    fn new(summary: &str, body: &str) -> Self
    {
        Self { summary: preview(summary, SUMMARY_CHAR_LIMIT), body: preview(body, BODY_CHAR_LIMIT) }
    }
}

#[derive(Default)]
struct Dispatcher
{
    sender: Mutex<Option<SyncSender<Notice>>>,
}

impl Dispatcher
{
    // Notification delivery can block indefinitely in the desktop service.
    // Never block the UI or create another thread for every incoming message.
    fn send(&self, notice: Notice, mut show: impl FnMut(Notice) + Send + 'static) -> bool
    {
        let mut sender = self.sender.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if sender.is_none() {
            let (tx, rx) = mpsc::sync_channel(QUEUE_CAPACITY);
            if let Err(error) = std::thread::Builder::new().name("snack-notifications".into()).spawn(move || {
                for notice in rx { show(notice); }
            }) {
                log::warn!("Could not start desktop notification worker: {error}");
                return false;
            }
            *sender = Some(tx);
        }
        match sender.as_ref().unwrap().try_send(notice) {
            Ok(()) => true,
            // Alerts are best effort. The actual messages remain in chat/history.
            Err(TrySendError::Full(_)) => false,
            Err(TrySendError::Disconnected(_)) => {
                // A crashed worker must not disable alerts permanently. The
                // next notification will start a new worker.
                *sender = None;
                false
            }
        }
    }
}

pub fn send(summary: &str, body: &str)
{
    static DISPATCHER: Dispatcher = Dispatcher { sender: Mutex::new(None) };
    if DISPATCHER.send(Notice::new(summary, body), show) {
        log::debug!("Queued desktop alert \"{summary}\"");
    } else {
        log::warn!("Dropped desktop alert \"{summary}\": the notification worker is busy or unavailable");
    }
}

fn prepare_notification(notice: &Notice, body_markup: Result<bool, String>) -> Result<notify_rust::Notification, String>
{
    let body = if body_markup? {
        // Escape once for XML character data. Literal entity spellings must
        // also survive one decode by the desktop service.
        notice.body.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    } else { notice.body.clone() };
    let mut notification = notify_rust::Notification::new();
    notification.summary(&notice.summary).body(&body);
    Ok(notification)
}

#[cfg(not(test))]
fn show(notice: Notice)
{
    // Freedesktop bodies can be XML markup; native macOS/Windows bodies are
    // plain text. Query on the worker for each alert so daemon restarts or a
    // failed capability lookup cannot leave a stale rendering mode cached.
    #[cfg(all(unix, not(target_os = "macos")))]
    let body_markup = notify_rust::get_capabilities()
        .map(|capabilities| capabilities.iter().any(|capability| capability == "body-markup"))
        .map_err(|error| format!("Could not inspect desktop notification capabilities: {error}"));
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    let body_markup = Ok(false);
    #[cfg(target_os = "macos")]
    if !macos::sender_ready() { return; }
    log::debug!("Showing desktop alert \"{}\" ({} characters)", notice.summary, notice.body.chars().count());
    let started = std::time::Instant::now();
    let result = prepare_notification(&notice, body_markup)
        .and_then(|notification| notification.show().map(|_| ()).map_err(|error| error.to_string()));
    match result {
        Ok(()) => log::debug!("Desktop alert \"{}\" accepted by the system after {:?}", notice.summary, started.elapsed()),
        Err(error) => log::warn!("Failed to send notification \"{}\": {error}", notice.summary),
    }
}

// macOS delivers a notification on behalf of an application bundle. Without
// one, the notification library asks AppleScript for an app named
// "use_default", which makes macOS show a "Where is use_default?" chooser;
// cancelling it leaves alerts without a valid sender. Choose a registered
// bundle up front: Snack's own when running from snack.app (or when one is
// installed), else the app that launched a development build, e.g. Terminal.
#[cfg(all(target_os = "macos", not(test)))]
mod macos
{
    use std::sync::OnceLock;

    fn registered(bundle: &str) -> bool
    {
        use objc2_app_kit::NSWorkspace;
        use objc2_foundation::NSString;
        unsafe { NSWorkspace::sharedWorkspace().URLForApplicationWithBundleIdentifier(&NSString::from_str(bundle)) }.is_some()
    }

    fn sender() -> Option<String>
    {
        use objc2_foundation::NSBundle;
        let own = unsafe { NSBundle::mainBundle().bundleIdentifier() }.map(|bundle| bundle.to_string());
        let launcher = std::env::var("__CFBundleIdentifier").ok();
        log::debug!("Notification sender candidates: own bundle {own:?}, launcher {launcher:?}");
        [own, Some("org.snack.snack".to_string()), launcher, Some("com.apple.Terminal".to_string())]
            .into_iter().flatten().find(|bundle| {
                let found = registered(bundle);
                log::debug!("Notification sender {bundle}: {}", if found { "registered" } else { "not registered" });
                found
            })
    }

    // The library allows setting its sender once per process.
    pub fn sender_ready() -> bool
    {
        static READY: OnceLock<bool> = OnceLock::new();
        *READY.get_or_init(|| match sender() {
            Some(bundle) => match notify_rust::set_application(&bundle) {
                Ok(()) => { log::debug!("Sending notifications as {bundle}"); true }
                Err(error) => { log::warn!("Cannot send notifications as {bundle}: {error}"); false }
            },
            None => { log::warn!("No registered application can send notifications; alerts are disabled"); false }
        })
    }
}

// Application tests may exercise unfocused-message behavior. Never display a
// real desktop alert or contact the user's notification service in a test.
#[cfg(test)]
fn show(_: Notice) {}

#[cfg(test)]
mod tests
{
    use super::*;
    use std::sync::{Arc, Condvar, atomic::{AtomicUsize, Ordering}};
    use std::time::Duration;

    #[test]
    fn markup_capable_notifications_display_message_content_literally()
    {
        let notice = Notice::new("<alice> & friends", "<b>literal</b> &amp; <img src='file:///private/image'/>");
        let notification = prepare_notification(&notice, Ok(true)).unwrap();
        assert_eq!(notification.summary, notice.summary);
        assert_eq!(notification.body, "&lt;b&gt;literal&lt;/b&gt; &amp;amp; &lt;img src='file:///private/image'/&gt;");
        let plain = prepare_notification(&notice, Ok(false)).unwrap();
        assert_eq!(plain.body, notice.body);
    }

    #[test]
    fn unknown_capabilities_do_not_send_potential_markup_and_can_be_retried()
    {
        let notice = Notice::new("title", "<a href='https://example.invalid'>literal link</a>");
        assert!(prepare_notification(&notice, Err("service unavailable".into())).is_err());
        let recovered = prepare_notification(&notice, Ok(true)).unwrap();
        assert_eq!(recovered.body, "&lt;a href='https://example.invalid'&gt;literal link&lt;/a&gt;");
        let plain_daemon = prepare_notification(&notice, Ok(false)).unwrap();
        assert_eq!(plain_daemon.body, notice.body);
    }

    #[test]
    fn markup_escaping_happens_after_preview_truncation_without_dangling_entities()
    {
        let notice = Notice::new("title", &"&".repeat(BODY_CHAR_LIMIT + 1));
        let markup = prepare_notification(&notice, Ok(true)).unwrap();
        assert_eq!(markup.body, format!("{}…", "&amp;".repeat(BODY_CHAR_LIMIT)));
    }

    #[test]
    fn slow_backend_cannot_create_unbounded_threads_or_block_message_processing()
    {
        let dispatcher = Arc::new(Dispatcher::default());
        let (started_tx, started_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let active = Arc::new(AtomicUsize::new(0));
        let worker_gate = gate.clone();
        let worker_active = active.clone();
        assert!(dispatcher.send(Notice::new("first", "body"), move |notice| {
            let previous = worker_active.fetch_add(1, Ordering::SeqCst);
            assert_eq!(previous, 0);
            started_tx.send(()).unwrap();
            let (ready, changed) = &*worker_gate;
            let mut ready = ready.lock().unwrap();
            while !*ready { ready = changed.wait(ready).unwrap(); }
            worker_active.fetch_sub(1, Ordering::SeqCst);
            finished_tx.send(notice.summary).unwrap();
        }));
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let submitter = dispatcher.clone();
        let (submitted_tx, submitted_rx) = mpsc::channel();
        let submission = std::thread::spawn(move || {
            let accepted = (0..1000).filter(|index| submitter.send(Notice::new(&format!("queued{index}"), "body"), |_| panic!("must reuse the existing worker"))).count();
            submitted_tx.send(accepted).unwrap();
        });
        let accepted = submitted_rx.recv_timeout(Duration::from_secs(1));
        let active_before_release = active.load(Ordering::SeqCst);
        // Release before asserting so a blocking-send regression fails the
        // deadline without stranding the submitter or notification worker.
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        submission.join().unwrap();
        let accepted = accepted.expect("notification queue blocked message processing");
        assert_eq!(accepted, QUEUE_CAPACITY);
        assert_eq!(active_before_release, 1);
        assert_eq!(finished_rx.recv_timeout(Duration::from_secs(1)).unwrap(), "first");
        for index in 0..accepted {
            assert_eq!(finished_rx.recv_timeout(Duration::from_secs(1)).unwrap(), format!("queued{index}"));
        }
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn previews_bound_unicode_payloads_without_modifying_short_messages()
    {
        let notice = Notice::new("short title", "normal body");
        assert_eq!(notice.summary, "short title");
        assert_eq!(notice.body, "normal body");
        let notice = Notice::new(&"🦀".repeat(1000), &"é".repeat(10000));
        assert_eq!(notice.summary, format!("{}…", "🦀".repeat(SUMMARY_CHAR_LIMIT)));
        assert_eq!(notice.body, format!("{}…", "é".repeat(BODY_CHAR_LIMIT)));
    }

    #[test]
    fn disconnected_worker_is_replaced_on_a_later_notification()
    {
        let (tx, rx) = mpsc::sync_channel(QUEUE_CAPACITY);
        drop(rx);
        let dispatcher = Dispatcher { sender: Mutex::new(Some(tx)) };
        assert!(!dispatcher.send(Notice::new("lost", "body"), |_| panic!("old sender must be detected")));
        let (shown_tx, shown_rx) = mpsc::channel();
        assert!(dispatcher.send(Notice::new("new worker", "body"), move |notice| { shown_tx.send(notice.summary).unwrap(); }));
        assert_eq!(shown_rx.recv_timeout(Duration::from_secs(1)).unwrap(), "new worker");
    }
}
