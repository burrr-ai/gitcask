//! Local git repository engine: gix in-process for odb/refs/revwalk; upstream
//! git subprocesses for ingest (`index-pack`), repack, and upload-pack.

mod advertise;
mod connectivity;
mod error;
mod import;
mod import_process;
pub use import_process::{ImportProcess, ImportProcesses};
mod ingest;
pub use import::isolated_command;
mod local;
mod maintain;
pub mod pkt;
mod proc;
pub mod receive;
mod refs;
mod repo_id;
mod types;

pub use advertise::build_v2_fetch_request;
pub use error::{GitError, validate_oid, validate_ref_name, validate_ref_update};
pub use gix_hash::{self, ObjectId};
pub use ingest::SpooledPack;
pub use local::LocalRepo;
pub use maintain::write_rev_from_idx;
pub use refs::RefView;
pub use repo_id::{ObjectFormat, RepoId};
pub use types::{
    FsckReport, IngestOptions, IngestedPack, LsRefsArgs, LsRefsLine, PackInfo, Ref,
    RefSnapshotData, RepackMode, RepackOptions, RepackResult, Service, UploadPackRequest,
};
