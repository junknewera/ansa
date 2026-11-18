use serde::{Deserialize, Serialize};
use std::path::Path;

pub const VERSION: u16 = 1;
pub const MAX_INPUT: usize = 4096;
pub const MAX_ROWS: u16 = 128;
pub const MAX_COLUMNS: u16 = 256;
pub const MAX_LIFETIME_SECONDS: u32 = 1800;
pub const MAX_IDLE_SECONDS: u32 = 300;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Id(String);

impl Id {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Id {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b':'))
        {
            return Err("terminal_id");
        }
        Ok(Self(value))
    }
}

impl From<Id> for String {
    fn from(value: Id) -> Self {
        value.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub session: Id,
    pub scope: Id,
    pub host: Id,
    pub workspace: String,
}

impl Binding {
    pub(crate) fn validate(&self) -> bool {
        absolute_path(&self.workspace)
    }

    pub fn terminal_id(&self, operation: &Id) -> Id {
        let identity = format!(
            "{}\0{}\0{}\0{}",
            self.session.as_str(),
            self.scope.as_str(),
            self.host.as_str(),
            operation.as_str()
        );
        Id::try_from(format!("pty_{}", sha256(identity.as_bytes()))).expect("fixed terminal ID")
    }
}

fn absolute_path(value: &str) -> bool {
    value.len() <= 1024
        && !value.chars().any(char::is_control)
        && Path::new(value).is_absolute()
        && ansa_linux::fs::normalize(Path::new(value)).as_os_str() == value
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Writer {
    Agent,
    Human,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Size {
    pub rows: u16,
    pub columns: u16,
}

impl Size {
    pub(crate) fn validate(self) -> bool {
        (1..=MAX_ROWS).contains(&self.rows) && (1..=MAX_COLUMNS).contains(&self.columns)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Launch {
    pub label: String,
    pub cwd: String,
    pub argv: Vec<String>,
    pub size: Size,
    pub lifetime_seconds: u32,
    pub idle_seconds: u32,
}

impl Launch {
    pub(crate) fn validate(&self) -> bool {
        !self.label.is_empty()
            && self.label.len() <= 64
            && !self.label.chars().any(char::is_control)
            && absolute_path(&self.cwd)
            && (1..=32).contains(&self.argv.len())
            && !self.argv[0].is_empty()
            && self
                .argv
                .iter()
                .all(|arg| !arg.chars().any(char::is_control))
            && self.argv.iter().map(String::len).sum::<usize>() <= 2048
            && self.size.validate()
            && (1..=MAX_LIFETIME_SECONDS).contains(&self.lifetime_seconds)
            && (1..=MAX_IDLE_SECONDS.min(self.lifetime_seconds)).contains(&self.idle_seconds)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovedScreen {
    pub through: u64,
    pub digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Create {
        launch: Launch,
    },
    Input {
        count: usize,
        digest: Option<String>,
    },
    Resize {
        size: Size,
    },
    Interrupt,
    Close,
    Takeover,
    Renew,
    Handback {
        screen: ApprovedScreen,
    },
}

impl Action {
    pub fn input(writer: Writer, bytes: &[u8]) -> Option<Self> {
        (1..=MAX_INPUT).contains(&bytes.len()).then(|| Self::Input {
            count: bytes.len(),
            digest: (writer == Writer::Agent).then(|| sha256(bytes)),
        })
    }

    pub(crate) fn validate(&self, writer: Writer) -> bool {
        match self {
            Self::Create { launch } => launch.validate(),
            Self::Input { count, digest } => {
                (1..=MAX_INPUT).contains(count)
                    && match writer {
                        Writer::Agent => digest.as_deref().is_some_and(valid_digest),
                        Writer::Human => digest.is_none(),
                    }
            }
            Self::Resize { size } => size.validate(),
            Self::Handback { screen } => valid_digest(&screen.digest),
            _ => true,
        }
    }
}

pub(crate) fn sha256(bytes: &[u8]) -> String {
    crate::protocol::legacy_json_v1::sha256(bytes)
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Intent {
    pub operation: Id,
    pub terminal: Id,
    pub binding: Binding,
    pub epoch: u64,
    pub writer: Writer,
    pub invocation_digest: Option<String>,
    pub action: Action,
}

impl Intent {
    pub(crate) fn validates(&self) -> bool {
        self.action.validate(self.writer)
            && match self.writer {
                Writer::Agent => self.invocation_digest.as_deref().is_some_and(valid_digest),
                Writer::Human => self.invocation_digest.is_none(),
            }
    }

    pub(crate) fn binds_payload(&self, input: &[u8]) -> bool {
        match &self.action {
            Action::Input { .. } => {
                Action::input(self.writer, input).as_ref() == Some(&self.action)
            }
            _ => input.is_empty(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Effect {
    Applied,
    Delivered { count: usize },
    Partial { count: usize },
    NotStarted,
    Unknown,
}

impl Effect {
    pub(crate) fn validates(self, action: &Action) -> bool {
        match (self, action) {
            (Self::NotStarted | Self::Unknown, _) => true,
            (
                Self::Delivered { count },
                Action::Input {
                    count: expected, ..
                },
            ) => count == *expected,
            (
                Self::Partial { count },
                Action::Input {
                    count: expected, ..
                },
            ) => count > 0 && count < *expected,
            (Self::Applied, Action::Input { .. }) => false,
            (Self::Applied, _) => true,
            _ => false,
        }
    }

    pub(crate) fn uncertain(self) -> bool {
        matches!(self, Self::Partial { .. } | Self::Unknown)
    }
}
