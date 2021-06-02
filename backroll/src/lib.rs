use std::time::Duration;
use thiserror::Error;

mod backend;
mod command;
mod input;
mod protocol;
mod sync;
mod time_sync;

pub use backend::*;
pub use backroll_transport as transport;
pub use command::Command;
pub use input::GameInput;

// TODO(james7132): Generalize the executor for these.
pub(crate) use bevy_tasks::TaskPool;

pub const MAX_PLAYERS_PER_MATCH: usize = 8;
// Approximately 2 seconds of frames.
const MAX_ROLLBACK_FRAMES: usize = 120;

type Frame = i32;
const NULL_FRAME: Frame = -1;

fn is_null(frame: Frame) -> bool {
    frame < 0
}

/// A handle for a player in a Backroll session.
#[derive(Copy, Clone, Debug)]
pub struct PlayerHandle(pub usize);

pub enum Player {
    /// The local player. Backroll currently only supports one local player per machine.
    Local,
    /// A non-participating peer that recieves inputs but does not send any.
    Spectator(transport::Peer),
    /// A remote player that is not on the local session.
    Remote(transport::Peer),
}

impl Player {
    pub(crate) fn is_local(&self) -> bool {
        matches!(self, Self::Local)
    }
}

pub trait Config: 'static {
    type Input: Eq + bytemuck::Pod + bytemuck::Zeroable + Send + Sync;

    /// The save state type for the session. This type must be safe to send across
    /// threads and have a 'static lifetime. This type is also responsible for
    /// dropping any internal linked state via the `[Drop]` trait.
    type State: 'static + Clone + Send + Sync;

    const MAX_PLAYERS_PER_MATCH: usize;
    const RECOMMENDATION_INTERVAL: u32;
}

#[derive(Clone, Debug, Error)]
pub enum BackrollError {
    #[error("Multiple players ")]
    MultipleLocalPlayers,
    #[error("Action cannot be taken while in rollback.")]
    InRollback,
    #[error("The session has not been synchronized yet.")]
    NotSynchronized,
    #[error("The simulation has reached the prediction barrier.")]
    ReachedPredictionBarrier,
    #[error("Invalid player handle: {:?}", .0)]
    InvalidPlayer(PlayerHandle),
    #[error("Player already disconnected: {:?}", .0)]
    PlayerDisconnected(PlayerHandle),
}

pub type BackrollResult<T> = Result<T, BackrollError>;

#[derive(Clone, Debug, Default)]
pub struct NetworkStats {
    pub ping: Duration,
    pub send_queue_len: usize,
    pub recv_queue_len: usize,
    pub kbps_sent: u32,

    pub local_frames_behind: Frame,
    pub remote_frames_behind: Frame,
}

#[derive(Clone, Debug)]
pub enum Event {
    /// A initial response packet from the remote player has been recieved.
    Connected(PlayerHandle),
    /// A response from a remote player has been recieved during the initial
    /// synchronization handshake.
    Synchronizing {
        player: PlayerHandle,
        count: u8,
        total: u8,
    },
    /// The initial synchronization handshake has been completed. The connection
    /// is considered live now.
    Synchronized(PlayerHandle),
    /// All remote peers are now synchronized, the session is can now start
    /// running.
    Running,
    /// The connection with a remote player has been disconnected.
    Disconnected(PlayerHandle),
    /// The local client is several frames ahead of all other peers. Might need
    /// to stall a few frames to allow others to catch up.
    TimeSync { frames_ahead: u8 },
    /// The connection with a remote player has been temporarily interrupted.
    ConnectionInterrupted {
        player: PlayerHandle,
        disconnect_timeout: Duration,
    },
    /// The connection with a remote player has been resumed after being interrupted.
    ConnectionResumed(PlayerHandle),
}
