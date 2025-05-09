use parking_lot::RwLock;
use regex::Regex;
use regex::RegexBuilder;
use rquest::Response;
use std::ops::Deref;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::{collections::HashMap, sync::Arc};
use tokio::time::sleep;
use tokio::{spawn, time::Duration};
use tracing::debug;
use tracing::error;
use tracing::warn;

use crate::client::AppendHeaders;
use crate::client::SUPER_CLIENT;
use crate::config::Config;
use crate::config::UselessReason;
use crate::error::ClewdrError;

/// Inner state of the application
///
/// Mutable fields are all Atomic or RwLock
///
/// Caution for deadlocks
#[derive(Default)]
pub struct InnerState {
    pub config: RwLock<Config>,
    init_length: u64,
    cons_requests: AtomicU64,
    rotating: AtomicBool,
    pub is_pro: RwLock<Option<String>>,
    pub uuid_org: RwLock<String>,
    cookies: RwLock<HashMap<String, String>>,
    pub uuid_org_array: RwLock<Vec<String>>,
    pub conv_uuid: RwLock<Option<String>>,
}

impl Deref for AppState {
    type Target = InnerState;
    /// Implement Deref trait for AppState for easier access to inner state
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

/// Arc wrapper for the inner state
///
/// Mutable fields are all Atomic or RwLock
///
/// Caution for deadlocks
#[derive(Clone)]
pub struct AppState {
    inner: Arc<InnerState>,
}

impl AppState {
    /// Create a new AppState instance
    pub fn new(config: Config) -> Self {
        let m = InnerState {
            init_length: config.cookie_array_len() as u64,
            config: RwLock::new(config),
            ..Default::default()
        };
        let m = Arc::new(m);
        AppState { inner: m }
    }

    /// increase the number of consequence requests
    pub fn increase_cons_requests(&self) {
        let mut cons_requests = self.cons_requests.load(Ordering::Relaxed);
        debug!("Current concurrent requests: {}", cons_requests);
        cons_requests += 1;
        let max_cons_requests = self.config.read().max_cons_requests;
        // if consequence requests is greater than max, rotate cookie
        if cons_requests > max_cons_requests {
            cons_requests = 0;
            warn!("Reached max concurrent requests, rotating cookie");
            self.cookie_rotate(UselessReason::CoolDown);
        }
        self.cons_requests.store(cons_requests, Ordering::Relaxed);
    }

    /// Update cookie from the server response
    pub fn update_cookie_from_res(&self, res: &Response) {
        if let Some(s) = res
            .headers()
            .get("set-cookie")
            .and_then(|h| h.to_str().ok())
        {
            self.update_cookies(s)
        }
    }

    /// Update cookies from string
    pub fn update_cookies(&self, str: &str) {
        let str = str.split("\n").to_owned().collect::<Vec<_>>().join("");
        if str.is_empty() {
            return;
        }
        let re1 = Regex::new(r";\s?").unwrap();
        let re2 = RegexBuilder::new(r"^(path|expires|domain|HttpOnly|Secure|SameSite)[=;]*")
            .case_insensitive(true)
            .build()
            .unwrap();
        let re3 = Regex::new(r"^(.*?)=\s*(.*)").unwrap();
        re1.split(&str)
            .filter(|s| !re2.is_match(s) && !s.is_empty())
            .for_each(|s| {
                let caps = re3.captures(s);
                if let Some(caps) = caps {
                    let key = caps[1].to_string();
                    let value = caps[2].to_string();
                    let mut cookies = self.cookies.write();
                    cookies.insert(key, value);
                }
            });
    }

    /// Current cookie string that are used in requests
    pub fn header_cookie(&self) -> Result<String, ClewdrError> {
        // check rotating guard
        if self.rotating.load(Ordering::Relaxed) {
            return Err(ClewdrError::CookieRotating);
        }
        let cookies = self.cookies.read();
        Ok(cookies
            .iter()
            .map(|(name, value)| format!("{}={}", name, value))
            .collect::<Vec<_>>()
            .join("; ")
            .trim()
            .to_string())
    }

    /// Rotate the cookie for the given reason
    pub fn cookie_rotate(&self, reason: UselessReason) {
        static SHIFTS: AtomicU64 = AtomicU64::new(0);
        if SHIFTS.load(Ordering::Relaxed) == self.init_length {
            error!("Cookie used up, not rotating");
            return;
        }
        // create scope to avoid deadlock
        {
            let mut config = self.config.write();
            let Some(current_cookie) = config.current_cookie_info() else {
                return;
            };
            match reason {
                UselessReason::CoolDown => {
                    warn!("Cookie is in cooling down, not cleaning");
                    config.rotate_cookie();
                }
                UselessReason::Exhausted(i) => {
                    warn!("Temporary useless cookie, not cleaning");
                    current_cookie.reset_time = Some(i);
                    config.save().unwrap_or_else(|e| {
                        error!("Failed to save config: {}", e);
                    });
                    config.rotate_cookie();
                }
                _ => {
                    // if reason is not temporary, clean cookie
                    config.cookie_cleaner(reason);
                }
            }
        }
        let config = self.config.read();
        // rotate the cookie
        config.save().unwrap_or_else(|e| {
            error!("Failed to save config: {}", e);
        });
        // set timeout callback
        let dur = if config.rproxy.is_empty() {
            let time = config.wait_time;
            warn!("Waiting {time} seconds to change cookie");
            time
        } else {
            0
        };
        let dur = Duration::from_secs(dur);
        let self_clone = self.clone();
        SHIFTS.fetch_add(1, Ordering::Relaxed);
        spawn(async move {
            self_clone.rotating.store(true, Ordering::Relaxed);
            self_clone.cons_requests.store(0, Ordering::Relaxed);
            sleep(dur).await;
            warn!("Cookie rotating complete");
            self_clone.rotating.store(false, Ordering::Relaxed);
            self_clone.bootstrap().await;
        });
    }

    /// Delete current chat conversation
    pub async fn delete_chat(&self) -> Result<(), ClewdrError> {
        let uuid = self.conv_uuid.write().take();
        let config = self.config.read().clone();
        let uuid_org = self.uuid_org.read().clone();
        if uuid.clone().is_none_or(|u| u.is_empty()) {
            return Ok(());
        }
        let uuid = uuid.unwrap();
        // if preserve_chats is true, do not delete chat
        if config.settings.preserve_chats {
            return Ok(());
        }
        debug!("Deleting chat: {}", uuid);
        let endpoint = format!(
            "{}/api/organizations/{}/chat_conversations/{}",
            config.endpoint(),
            uuid_org,
            uuid
        );
        let res = SUPER_CLIENT
            .delete(endpoint.clone())
            .append_headers(
                "",
                self.header_cookie()?,
                self.config.read().rquest_proxy.clone(),
            )
            .send()
            .await?;
        self.update_cookie_from_res(&res);
        debug!("Chat deleted");
        Ok(())
    }
}
