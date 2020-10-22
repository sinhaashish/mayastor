use std::{convert::TryFrom, fmt::Display, sync::Arc};

use nix::errno::Errno;
use serde::{export::Formatter, Serialize};
use snafu::{ResultExt, Snafu};

use spdk_sys::{spdk_bdev_module_release_bdev, spdk_io_channel};

use crate::{
    bdev::{
        nexus::{
            nexus_child::ChildState::Faulted,
            nexus_child_status_config::ChildStatusConfig,
        },
        NexusErrStore,
    },
    core::{Bdev, BdevHandle, CoreError, Descriptor, DmaBuf},
    nexus_uri::{bdev_destroy, NexusBdevError},
    rebuild::{ClientOperations, RebuildJob},
    subsys::Config,
};

#[derive(Debug, Snafu)]
pub enum ChildError {
    #[snafu(display("Child is not offline"))]
    ChildNotOffline {},
    #[snafu(display("Child is not closed"))]
    ChildNotClosed {},
    #[snafu(display("Child is faulted, it cannot be reopened"))]
    ChildFaulted {},
    #[snafu(display(
        "Child is smaller than parent {} vs {}",
        child_size,
        parent_size
    ))]
    ChildTooSmall { child_size: u64, parent_size: u64 },
    #[snafu(display("Open child"))]
    OpenChild { source: CoreError },
    #[snafu(display("Claim child"))]
    ClaimChild { source: Errno },
    #[snafu(display("Child is inaccessible"))]
    ChildInaccessible {},
    #[snafu(display("Invalid state of child"))]
    ChildInvalid {},
    #[snafu(display("Opening child bdev without bdev pointer"))]
    OpenWithoutBdev {},
    #[snafu(display("Failed to create a BdevHandle for child"))]
    HandleCreate { source: CoreError },
}

#[derive(Debug, Snafu)]
pub enum ChildIoError {
    #[snafu(display("Error writing to {}: {}", name, source))]
    WriteError { source: CoreError, name: String },
    #[snafu(display("Error reading from {}: {}", name, source))]
    ReadError { source: CoreError, name: String },
    #[snafu(display("Invalid descriptor for child bdev {}", name))]
    InvalidDescriptor { name: String },
}

#[derive(Debug, Serialize, PartialEq, Deserialize, Copy, Clone)]
pub enum Reason {
    /// no particular reason for the child to be in this state
    /// this is typically the init state
    Unknown,
    /// out of sync - needs to be rebuilt
    OutOfSync,
    /// cannot open
    CantOpen,
    /// the child failed to rebuild successfully
    RebuildFailed,
    /// the child has been faulted due to I/O error(s)
    IoError,
    /// the child has been explicitly faulted due to a rpc call
    Rpc,
}

impl Display for Reason {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => write!(f, "Unknown"),
            Self::OutOfSync => {
                write!(f, "The child is out of sync and requires a rebuild")
            }
            Self::CantOpen => write!(f, "The child bdev could not be opened"),
            Self::RebuildFailed => {
                write!(f, "The child failed to rebuild successfully")
            }
            Self::IoError => write!(f, "The child had too many I/O errors"),
            Self::Rpc => write!(f, "The child is faulted due to a rpc call"),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum ChildState {
    /// child has not been opened, but we are in the process of opening it
    Init,
    /// cannot add this bdev to the parent as its incompatible property wise
    ConfigInvalid,
    /// the child is open for RW
    Open,
    /// the child has been closed by the nexus
    Closed,
    /// the child is faulted
    Faulted(Reason),
}

impl Display for ChildState {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Faulted(r) => write!(f, "Faulted with reason {}", r),
            Self::Init => write!(f, "Init"),
            Self::ConfigInvalid => write!(f, "Config parameters are invalid"),
            Self::Open => write!(f, "Child is open"),
            Self::Closed => write!(f, "Closed"),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct NexusChild {
    /// name of the parent this child belongs too
    pub(crate) parent: String,
    /// Name of the child is the URI used to create it.
    /// Note that bdev name can differ from it!
    pub(crate) name: String,
    #[serde(skip_serializing)]
    /// the bdev wrapped in Bdev
    pub(crate) bdev: Option<Bdev>,
    #[serde(skip_serializing)]
    /// channel on which we submit the IO
    pub(crate) ch: *mut spdk_io_channel,
    #[serde(skip_serializing)]
    pub(crate) desc: Option<Arc<Descriptor>>,
    /// current state of the child
    #[serde(skip_serializing)]
    state: ChildState,
    /// descriptor obtained after opening a device
    #[serde(skip_serializing)]
    pub(crate) bdev_handle: Option<BdevHandle>,
    /// record of most-recent IO errors
    #[serde(skip_serializing)]
    pub(crate) err_store: Option<NexusErrStore>,
}

impl Display for NexusChild {
    fn fmt(&self, f: &mut Formatter) -> Result<(), std::fmt::Error> {
        if self.bdev.is_some() {
            let bdev = self.bdev.as_ref().unwrap();
            writeln!(
                f,
                "{}: {:?}, blk_cnt: {}, blk_size: {}",
                self.name,
                self.state(),
                bdev.num_blocks(),
                bdev.block_len(),
            )
        } else {
            writeln!(f, "{}: state {:?}", self.name, self.state())
        }
    }
}

impl NexusChild {
    pub(crate) fn set_state(&mut self, state: ChildState) {
        trace!(
            "{}: child {}: state change from {} to {}",
            self.parent,
            self.name,
            self.state.to_string(),
            state.to_string(),
        );

        self.state = state;
    }

    /// Open the child in RW mode and claim the device to be ours. If the child
    /// is already opened by someone else (i.e one of the targets) it will
    /// error out.
    ///
    /// only devices in the closed or Init state can be opened.
    ///
    /// A child can only be opened if:
    ///  - it's not faulted
    ///  - it's not already opened
    pub(crate) fn open(
        &mut self,
        parent_size: u64,
    ) -> Result<String, ChildError> {
        trace!("{}: Opening child device {}", self.parent, self.name);

        // verify the state of the child before we open it
        match self.state() {
            ChildState::Faulted(reason) => {
                error!(
                    "{}: can not open child {} reason {}",
                    self.parent, self.name, reason
                );
                return Err(ChildError::ChildFaulted {});
            }
            ChildState::Open => {
                // the child (should) already be open
                assert_eq!(self.bdev.is_some(), true);
            }
            _ => {}
        }

        let bdev = self.bdev.as_ref().unwrap();

        let child_size = bdev.size_in_bytes();
        if parent_size > child_size {
            error!(
                "{}: child {} too small, parent size: {} child size: {}",
                self.parent, self.name, parent_size, child_size
            );

            self.set_state(ChildState::ConfigInvalid);
            return Err(ChildError::ChildTooSmall {
                parent_size,
                child_size,
            });
        }

        let desc = Arc::new(Bdev::open_by_name(&bdev.name(), true).map_err(
            |source| {
                self.set_state(Faulted(Reason::CantOpen));
                ChildError::OpenChild {
                    source,
                }
            },
        )?);

        self.bdev_handle = Some(BdevHandle::try_from(desc.clone()).unwrap());
        self.desc = Some(desc);

        let cfg = Config::get();
        if cfg.err_store_opts.enable_err_store {
            self.err_store =
                Some(NexusErrStore::new(cfg.err_store_opts.err_store_size));
        };

        self.set_state(ChildState::Open);

        debug!("{}: child {} opened successfully", self.parent, self.name);
        Ok(self.name.clone())
    }

    /// Fault the child with a specific reason.
    /// We do not close the child if it is out-of-sync because it will
    /// subsequently be rebuilt.
    pub(crate) fn fault(&mut self, reason: Reason) {
        match reason {
            Reason::OutOfSync => {
                self.set_state(ChildState::Faulted(reason));
            }
            _ => {
                self._close();
                self.set_state(ChildState::Faulted(reason));
            }
        }
        NexusChild::save_state_change();
    }

    /// Set the child as temporarily offline
    /// TODO: channels need to be updated when bdevs are closed
    pub(crate) fn offline(&mut self) {
        self.close();
        NexusChild::save_state_change();
    }

    /// Online a previously offlined child.
    /// The child is set out-of-sync so that it will be rebuilt.
    /// TODO: channels need to be updated when bdevs are opened
    pub(crate) fn online(
        &mut self,
        parent_size: u64,
    ) -> Result<String, ChildError> {
        let result = self.open(parent_size);
        self.set_state(ChildState::Faulted(Reason::OutOfSync));
        NexusChild::save_state_change();
        result
    }

    /// Save the state of the children to the config file
    pub(crate) fn save_state_change() {
        if ChildStatusConfig::save().is_err() {
            error!("Failed to save child status information");
        }
    }

    /// returns the state of the child
    pub fn state(&self) -> ChildState {
        self.state
    }

    pub(crate) fn rebuilding(&self) -> bool {
        match RebuildJob::lookup(&self.name) {
            Ok(_) => self.state() == ChildState::Faulted(Reason::OutOfSync),
            Err(_) => false,
        }
    }

    /// return a descriptor to this child
    pub fn get_descriptor(&self) -> Result<Arc<Descriptor>, CoreError> {
        if let Some(ref d) = self.desc {
            Ok(d.clone())
        } else {
            Err(CoreError::InvalidDescriptor {
                name: self.name.clone(),
            })
        }
    }

    /// closed the descriptor and handle, does not destroy the bdev
    fn _close(&mut self) {
        trace!("{}: Closing child {}", self.parent, self.name);
        if let Some(bdev) = self.bdev.as_ref() {
            unsafe {
                if !(*bdev.as_ptr()).internal.claim_module.is_null() {
                    spdk_bdev_module_release_bdev(bdev.as_ptr());
                }
            }
        }
        // just to be explicit
        let hdl = self.bdev_handle.take();
        let desc = self.desc.take();
        drop(hdl);
        drop(desc);
    }

    /// close the bdev -- we have no means of determining if this succeeds
    pub(crate) fn close(&mut self) -> ChildState {
        self._close();
        self.set_state(ChildState::Closed);
        ChildState::Closed
    }

    /// create a new nexus child
    pub fn new(name: String, parent: String, bdev: Option<Bdev>) -> Self {
        NexusChild {
            name,
            bdev,
            parent,
            desc: None,
            ch: std::ptr::null_mut(),
            state: ChildState::Init,
            bdev_handle: None,
            err_store: None,
        }
    }

    /// destroy the child bdev
    pub(crate) async fn destroy(&mut self) -> Result<(), NexusBdevError> {
        trace!("destroying child {:?}", self);
        assert_eq!(self.state(), ChildState::Closed);
        if let Some(_bdev) = &self.bdev {
            bdev_destroy(&self.name).await
        } else {
            warn!("Destroy child without bdev");
            Ok(())
        }
    }

    /// Check if the child is in a state that can service I/O.
    /// When out-of-sync, the child is still accessible (can accept I/O)
    /// because:
    /// 1. An added child starts in the out-of-sync state and may require its
    ///    label and metadata to be updated
    /// 2. It needs to be rebuilt
    fn is_accessible(&self) -> bool {
        self.state() == ChildState::Open
            || self.state() == ChildState::Faulted(Reason::OutOfSync)
    }

    /// return references to child's bdev and descriptor
    /// both must be present - otherwise it is considered an error
    pub fn get_dev(&self) -> Result<(&Bdev, &BdevHandle), ChildError> {
        if !self.is_accessible() {
            info!("{}: Child is inaccessible: {}", self.parent, self.name);
            return Err(ChildError::ChildInaccessible {});
        }

        if let Some(bdev) = &self.bdev {
            if let Some(desc) = &self.bdev_handle {
                return Ok((bdev, desc));
            }
        }

        Err(ChildError::ChildInvalid {})
    }

    /// write the contents of the buffer to this child
    pub async fn write_at(
        &self,
        offset: u64,
        buf: &DmaBuf,
    ) -> Result<usize, ChildIoError> {
        match self.bdev_handle.as_ref() {
            Some(desc) => {
                Ok(desc.write_at(offset, buf).await.context(WriteError {
                    name: self.name.clone(),
                })?)
            }
            None => Err(ChildIoError::InvalidDescriptor {
                name: self.name.clone(),
            }),
        }
    }

    /// read from this child device into the given buffer
    pub async fn read_at(
        &self,
        offset: u64,
        buf: &mut DmaBuf,
    ) -> Result<u64, ChildIoError> {
        match self.bdev_handle.as_ref() {
            Some(desc) => {
                Ok(desc.read_at(offset, buf).await.context(ReadError {
                    name: self.name.clone(),
                })?)
            }
            None => Err(ChildIoError::InvalidDescriptor {
                name: self.name.clone(),
            }),
        }
    }

    /// Return the rebuild job which is rebuilding this child, if rebuilding
    fn get_rebuild_job(&self) -> Option<&mut RebuildJob> {
        let job = RebuildJob::lookup(&self.name).ok()?;
        assert_eq!(job.nexus, self.parent);
        Some(job)
    }

    /// Return the rebuild progress on this child, if rebuilding
    pub fn get_rebuild_progress(&self) -> i32 {
        self.get_rebuild_job()
            .map(|j| j.stats().progress as i32)
            .unwrap_or_else(|| -1)
    }

    /// Determines if a child is local to the nexus (i.e. on the same node)
    pub fn is_local(&self) -> Option<bool> {
        match &self.bdev {
            Some(bdev) => {
                // A local child is not exported over nvme or iscsi
                let local = bdev.driver() != "nvme" && bdev.driver() != "iscsi";
                Some(local)
            }
            None => None,
        }
    }
}
