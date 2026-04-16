use chrono::{Local, DateTime};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityEvent {
    pub timestamp: String,
    pub epoch_ms: i64,
    pub event_type: String,
    pub detail: String,
    pub char_count: Option<u32>,
    pub time_delta_ms: Option<f64>,
    pub severity: String,
}

impl ActivityEvent {
    pub fn new(
        event_type: &str,
        detail: &str,
        char_count: Option<u32>,
        time_delta_ms: Option<f64>,
    ) -> Self {
        let now: DateTime<Local> = Local::now();
        let severity = Self::classify_severity(event_type, char_count);
        Self {
            timestamp: now.format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
            epoch_ms: now.timestamp_millis(),
            event_type: event_type.to_string(),
            detail: detail.to_string(),
            char_count,
            time_delta_ms,
            severity,
        }
    }

    fn classify_severity(event_type: &str, char_count: Option<u32>) -> String {
        match event_type {
            "clipboard_external" => "warning".to_string(),
            "focus_lost" => "warning".to_string(),
            "paste_large" => "alert".to_string(),
            "tamper_detected" | "tamper_new_file" | "tamper_deleted" => "alert".to_string(),
            "paste" => {
                if char_count.unwrap_or(0) > 50 {
                    "warning".to_string()
                } else {
                    "info".to_string()
                }
            }
            _ => "info".to_string(),
        }
    }
}

#[derive(Clone)]
pub struct LogHandle {
    events: Arc<Mutex<Vec<ActivityEvent>>>,
}

impl LogHandle {
    pub fn add_event(&self, event: ActivityEvent) {
        if let Ok(mut events) = self.events.lock() {
            events.push(event);
        }
    }
}

pub struct ActivityLog {
    events: Arc<Mutex<Vec<ActivityEvent>>>,
}

impl ActivityLog {
    pub fn new() -> Self {
        Self {
            events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn get_handle(&self) -> LogHandle {
        LogHandle {
            events: Arc::clone(&self.events),
        }
    }

    pub fn get_events(&self) -> Vec<ActivityEvent> {
        self.events.lock().unwrap().clone()
    }

    pub fn add_event(&mut self, event: ActivityEvent) {
        self.events.lock().unwrap().push(event);
    }

    pub fn clear(&mut self) {
        self.events.lock().unwrap().clear();
    }
}
