//! HTTP: requests that retry and pace themselves, and [`Service`], a website that is asked
//! politely.

use crate::settings::settings;
use crate::{log, R};
use serde::de::DeserializeOwned;
use std::{
    error::Error,
    fmt,
    sync::{
        atomic::{AtomicBool, Ordering::Relaxed},
        LazyLock, Mutex, PoisonError,
    },
    time::{Duration, Instant},
};

/// A request that got no usable answer.
#[derive(Debug)]
pub(crate) enum HttpError {
    /// The server answered with this status.
    Status(u16, String),
    /// There was no answer: a timeout, or a refused or lost connection.
    Transport(String),
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Status(code, url) => write!(f, "HTTP {code} for {url}"),
            Self::Transport(message) => f.write_str(message),
        }
    }
}

impl Error for HttpError {}

impl HttpError {
    /// The status of an error from this module's requests, if the server answered.
    pub fn status(error: &(dyn Error + Send + Sync + 'static)) -> Option<u16> {
        match error.downcast_ref::<Self>()? {
            Self::Status(code, _) => Some(*code),
            Self::Transport(_) => None,
        }
    }
}

/// Spaces the requests to one site `interval_ms` apart, across all threads.
pub(crate) struct Pace(Mutex<Option<Instant>>, u64);

impl Pace {
    pub const fn new(interval_ms: u64) -> Self {
        Self(Mutex::new(None), interval_ms)
    }

    pub fn wait(&self) {
        let slot = {
            let mut next = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            let slot = next.map_or(Instant::now(), |next| next.max(Instant::now()));
            *next = Some(slot + Duration::from_millis(self.1));
            slot
        };
        std::thread::sleep(slot.saturating_duration_since(Instant::now()));
    }

    /// Makes every caller wait `pause` before the next request, for a service that asked us to slow
    /// down. Returns whether this lengthened the wait: a thread that was told the same moment finds
    /// it held already.
    pub fn hold(&self, pause: Duration) -> bool {
        let mut next = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let until = next
            .unwrap_or_else(Instant::now)
            .max(Instant::now() + pause);
        let longer = next.is_none_or(|slot| until > slot);
        *next = Some(until);
        longer
    }
}

static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .http_status_as_error(false)
        .max_idle_connections_per_host(64)
        // `--proxy`, or else the usual HTTPS_PROXY / ALL_PROXY variables, as Python spotDL's
        // requests reads them.
        .proxy(
            settings()
                .proxy
                .as_deref()
                .and_then(|proxy| ureq::Proxy::new(proxy).ok())
                .or_else(ureq::Proxy::try_from_env),
        )
        .build();
    ureq::Agent::new_with_config(config)
});

/// GET, retrying connection errors, 429 and 5xx with exponential backoff (`--max-retries` times).
pub(crate) fn get(url: &str, headers: &[(&str, &str)]) -> R<Vec<u8>> {
    request(url, headers, None, settings().max_retries, None)
}

/// Where `url` leads after its redirects.
pub(crate) fn final_url(url: &str) -> R<String> {
    Ok(ureq::ResponseExt::get_uri(&AGENT.get(url).call()?).to_string())
}

/// [`get`], parsed as JSON.
pub(crate) fn get_json<T: DeserializeOwned>(url: &str, headers: &[(&str, &str)]) -> R<T> {
    Ok(serde_json::from_slice(&get(url, headers)?)?)
}

/// POSTs `body`, as [`get`] does, and parses the reply as JSON.
pub(crate) fn post_json<T: DeserializeOwned>(
    url: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> R<T> {
    let reply = request(url, headers, Some(body), settings().max_retries, None)?;
    Ok(serde_json::from_slice(&reply)?)
}

/// The pause before retry `attempt`: the server's Retry-After, else doubling. Capped, so that a
/// dead host or a long ban under a high `--max-retries` fails within minutes rather than hanging
/// each request for half an hour.
fn backoff(attempt: u64, retry_after: Option<u64>) -> Duration {
    Duration::from_secs(retry_after.unwrap_or(1 << attempt.min(5)).min(60))
}

/// GET (or POST, given a `body`) with `retries` retries of connection errors, 429 and 5xx, spaced
/// by `pace`: a 429 holds it, so that every thread waits the pause out together rather than each
/// asking again at its own time and keeping the limit on.
pub(crate) fn request(
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
    retries: u64,
    pace: Option<&Pace>,
) -> R<Vec<u8>> {
    let mut attempt = 0;
    loop {
        if let Some(pace) = pace {
            pace.wait();
        }
        let method = if body.is_some() { "POST" } else { "GET" };
        let request = ureq::http::Request::builder().method(method).uri(url);
        let request = headers.iter().fold(request, |request, (key, value)| {
            request.header(*key, *value)
        });
        let reply = AGENT.run(request.body(body.unwrap_or_default().as_bytes().to_vec())?);
        let status = reply.as_ref().map(|reply| reply.status()).ok();
        let retry = status.is_none_or(|status| status == 429 || status.is_server_error());
        if retry && attempt < retries {
            attempt += 1;
            let after = reply
                .as_ref()
                .ok()
                .and_then(|reply| reply.headers().get("retry-after"));
            let pause = backoff(
                attempt,
                after.and_then(|after| after.to_str().ok()?.trim().parse().ok()),
            );
            match pace {
                Some(pace) if status.is_some_and(|status| status == 429) => drop(pace.hold(pause)),
                _ => std::thread::sleep(pause),
            }
            continue;
        }
        let mut reply = reply.map_err(|error| HttpError::Transport(error.to_string()))?;
        if !reply.status().is_success() {
            return Err(HttpError::Status(reply.status().as_u16(), url.into()).into());
        }
        return Ok(reply
            .body_mut()
            .with_config()
            .limit(1 << 28)
            .read_to_vec()?);
    }
}

/// How often [`Service::get`] asks before it gives up.
const ATTEMPTS: u64 = 12;

/// How many times a request waits out "too many requests": the first pause, then twice and four
/// times as long and so on. A banned run so fails each song within a few minutes and says so,
/// instead of waiting silently for an hour.
const BUSY_TRIES: u64 = 4;

/// A website that is asked politely: all threads share one pace, wait together when it says "too
/// many requests" or that it is overloaded, and stop asking when it blocks this client.
pub(crate) struct Service {
    name: &'static str,
    pace: Pace,
    /// Whether a 403 means that the site blocks this client.
    blocks_on_forbidden: bool,
    blocked: AtomicBool,
    /// The first pause after "too many requests".
    busy: Duration,
}

impl Service {
    pub const fn new(name: &'static str, interval_ms: u64, blocks_on_forbidden: bool) -> Self {
        let busy = Duration::from_secs(15);
        Self {
            name,
            pace: Pace::new(interval_ms),
            blocks_on_forbidden,
            blocked: AtomicBool::new(false),
            busy,
        }
    }

    #[cfg(test)]
    const fn with_busy(mut self, busy: Duration) -> Self {
        self.busy = busy;
        self
    }

    /// Whether the site blocked this client: it is not asked again in this run (asking again would
    /// be working around a block, not waiting out a limit).
    pub fn blocked(&self) -> bool {
        self.blocked.load(Relaxed)
    }

    /// Stops asking the site in this run, and says why, once.
    pub fn block(&self, reason: &str) {
        if !self.blocked.swap(true, Relaxed) {
            log(
                "WARNING",
                &format!("{}: {reason}: it is not asked again in this run", self.name),
            );
        }
    }

    /// Waits for this service's turn, for a caller that makes its own requests.
    pub fn wait(&self) {
        self.pace.wait();
    }

    /// The body of `url`, or nothing: the site has no such page (404, said nowhere), blocks us (see
    /// [`blocked`]), or still fails after the waits (a warning says so). A request the site refuses
    /// as too frequent waits for every thread, [`BUSY_TRIES`] times and longer each time; an
    /// overloaded site (5xx) is asked again after a short wait, a lost connection after a longer
    /// one. Only this loop retries: [`request`]'s own retries would ask again within seconds,
    /// unpaced and during a hold, and so keep the limit on.
    ///
    /// [`blocked`]: Self::blocked
    pub fn get(&self, url: &str, headers: &[(&str, &str)]) -> Option<Vec<u8>> {
        for attempt in 0..ATTEMPTS {
            if self.blocked() {
                return None;
            }
            self.pace.wait();
            let error = match request(url, headers, None, 0, None) {
                Ok(body) => return Some(body),
                Err(error) => error,
            };
            match HttpError::status(&*error) {
                Some(404) => return None,
                Some(403) if self.blocks_on_forbidden => {
                    self.block(&error.to_string());
                    return None;
                }
                Some(429) if attempt >= BUSY_TRIES => {
                    return self.give_up(url, "still too many requests")
                }
                Some(429) => {
                    let pause = self.busy * (1 << attempt);
                    if self.pace.hold(pause) {
                        log(
                            "WARNING",
                            &format!(
                                "{}: too many requests, waiting {} s",
                                self.name,
                                pause.as_secs()
                            ),
                        );
                    }
                }
                Some(500..) => drop(self.pace.hold(Duration::from_secs(2 + 2 * attempt.min(10)))),
                None if attempt < 4 => std::thread::sleep(Duration::from_secs(2 << attempt)),
                _ => return self.give_up(url, &error.to_string()),
            }
        }
        self.give_up(url, "no answer")
    }

    fn give_up(&self, url: &str, why: &str) -> Option<Vec<u8>> {
        log("WARNING", &format!("{}: {why} ({url})", self.name));
        None
    }
}

#[cfg(test)]
#[path = "fuzz/http.rs"]
mod fuzz;

#[cfg(test)]
mod tests {
    use super::{backoff, request, HttpError, Pace, Service};
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{
            atomic::{AtomicUsize, Ordering::Relaxed},
            Arc,
        },
        time::{Duration, Instant},
    };

    fn settings() {
        crate::settings::use_defaults();
    }

    /// A local server that answers the n-th request with `replies[n]` (the last one after that),
    /// and how many requests it got.
    fn serve(replies: &'static [&'static str]) -> (String, Arc<AtomicUsize>) {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", server.local_addr().unwrap());
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        std::thread::spawn(move || {
            for stream in server.incoming() {
                let mut stream = stream.unwrap();
                let _ = stream.read(&mut [0; 4096]);
                let reply = replies[seen.fetch_add(1, Relaxed).min(replies.len() - 1)];
                let _ = write!(
                    stream,
                    "HTTP/1.1 {reply}\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                );
            }
        });
        (url, count)
    }

    #[test]
    fn retry_pauses_are_capped() {
        assert_eq!(backoff(1, None), Duration::from_secs(2));
        // `--max-retries 10` against a dead host: not 1024 s for the last retry.
        assert_eq!(backoff(10, None), Duration::from_secs(32));
        assert_eq!(backoff(1, Some(5)), Duration::from_secs(5));
        assert_eq!(backoff(1, Some(86_400)), Duration::from_secs(60));
    }

    #[test]
    fn a_429_holds_every_thread_for_its_retry_after() {
        settings();
        let (url, _) = serve(&["429 Too Many Requests\r\nRetry-After: 1", "200 OK"]);
        static PACE: Pace = Pace::new(0);
        let start = Instant::now();
        let other = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            PACE.wait();
            start.elapsed()
        });
        assert_eq!(request(&url, &[], None, 3, Some(&PACE)).unwrap(), b"ok");
        // One second as the server asked, not the two of the doubling pause.
        assert!(
            (900..1900).contains(&start.elapsed().as_millis()),
            "{:?}",
            start.elapsed()
        );
        // Another thread that asks during the hold waits it out too.
        assert!(other.join().unwrap() >= Duration::from_millis(900));
    }

    #[test]
    fn errors_say_their_status() {
        settings();
        let (url, _) = serve(&["418 I'm a teapot"]);
        let error = request(&url, &[], None, 0, None).unwrap_err();
        assert_eq!(HttpError::status(&*error), Some(418));
        assert_eq!(error.to_string(), format!("HTTP 418 for {url}"));
        let error = request("http://127.0.0.1:1/", &[], None, 0, None).unwrap_err();
        assert_eq!(HttpError::status(&*error), None);
    }

    /// A banned run waited about 40 minutes for each Genius request, saying nothing: a request
    /// waits four times, and then gives up.
    #[test]
    fn a_service_that_stays_busy_is_waited_out_four_times_and_then_given_up() {
        settings();
        let (url, count) = serve(&["429 Too Many Requests"]);
        let service = Service::new("Test", 0, false).with_busy(Duration::from_millis(1));
        assert_eq!(service.get(&url, &[]), None);
        assert_eq!(count.load(Relaxed), 5);
    }

    #[test]
    fn a_service_asks_again_after_a_busy_moment() {
        settings();
        let (url, count) = serve(&["429 Too Many Requests", "503 Service Unavailable", "200 OK"]);
        let service = Service::new("Test", 0, false).with_busy(Duration::from_millis(1));
        assert_eq!(service.get(&url, &[]).unwrap(), b"ok");
        assert_eq!(count.load(Relaxed), 3);
    }

    #[test]
    fn a_missing_page_is_not_asked_again_and_a_block_stops_every_request() {
        settings();
        let (url, count) = serve(&["404 Not Found"]);
        let service = Service::new("Test", 0, true);
        assert_eq!(service.get(&url, &[]), None);
        assert_eq!((count.load(Relaxed), service.blocked()), (1, false));
        let (url, count) = serve(&["403 Forbidden"]);
        assert_eq!(service.get(&url, &[]), None);
        assert!(service.blocked());
        assert_eq!(service.get(&url, &[]), None);
        assert_eq!(count.load(Relaxed), 1);
    }

    /// Without the block rule a 403 is an ordinary refusal: one warning, no retry.
    #[test]
    fn a_403_is_an_ordinary_refusal_unless_the_service_blocks_on_it() {
        settings();
        let (url, count) = serve(&["403 Forbidden"]);
        let service = Service::new("Test", 0, false);
        assert_eq!(service.get(&url, &[]), None);
        assert_eq!((count.load(Relaxed), service.blocked()), (1, false));
    }
}
