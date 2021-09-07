use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

#[derive(Serialize, Debug)]
pub(crate) struct Transcription {
  room_name: String,
  room_url: String,
  start_time: DateTime<Utc>,
  end_time: DateTime<Utc>,
  events: Vec<TranscriptionEvent>,
}

#[derive(Serialize, Debug, Clone)]
#[serde(tag = "event", rename_all = "UPPERCASE")]
pub(crate) enum TranscriptionEvent {
  Join {
    timestamp: DateTime<Utc>,
    participant: Participant,
  },
  Leave {
    timestamp: DateTime<Utc>,
    participant: Participant,
  },
  Speech {
    timestamp: DateTime<Utc>,
    participant: Participant,
    is_interim: bool,
    stability: f64,
    language: String,
    message_id: String,
    transcript: Vec<Transcript>,
  },
}

impl TranscriptionEvent {
  pub(crate) fn is_interim(&self) -> bool {
    match self {
      TranscriptionEvent::Speech { is_interim, .. } => *is_interim,
      _ => false,
    }
  }
}

#[derive(Serialize, Debug, Clone, Default)]
pub(crate) struct Participant {
  pub(crate) id: String,
  pub(crate) name: String,
  pub(crate) avatar_url: String,
}

#[derive(Serialize, Debug, Clone, Default)]
pub(crate) struct Transcript {
  pub(crate) text: String,
  pub(crate) confidence: f64,
}

#[derive(Serialize, Debug)]
pub(crate) struct TranscriptionResult {
  pub(crate) r#type: String,
  #[serde(flatten)]
  pub(crate) event: TranscriptionEvent,
}
