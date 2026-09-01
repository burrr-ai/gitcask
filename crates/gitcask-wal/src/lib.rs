//! WAL: index (`manifest.pb`), immutable entries, linearizable publish via CAS,
//! catch-up/materialize of local repos, ref snapshots. See AGENTS.md §2 and AGENTS.md §2.

mod checkpoint;
mod error;
mod handle;
pub mod lockwait;
mod log_reader;
pub mod progress;
mod publish;
mod registry;
mod state;
mod sync;
pub mod tasks;

pub use checkpoint::{CheckpointTrigger, checkpoint_due};
pub use error::{CoordError, RefError, WalError};
pub use handle::RepoHandle;
pub use progress::{Progress, Reporter};
pub use publish::PublishResult;
pub use registry::{EvictReport, Registry};
pub use sync::{ReadGuard, SyncLevel};
pub use tasks::{Begin, TaskHandle, TaskRecord, Tasks};
