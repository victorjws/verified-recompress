//! Resource budgets.
//!
//! Concurrency is not expressed as a job count. It falls out of four independent
//! budgets — local disk, remote quota, network transfers, CPU cores — each held
//! only for the stage that needs it. A file waiting on a core is not holding a
//! network slot, so downloads, encodes and uploads overlap without any explicit
//! stage scheduling.
//!
//! Sizes are counted in MiB throughout. `Semaphore::acquire_many` takes a `u32`,
//! so byte-denominated permits would overflow on any file past 4 GiB.

use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::Config;
use crate::policy::Recipe;

pub const MIB: u64 = 1024 * 1024;

/// Rounds a byte count up to whole MiB, saturating rather than wrapping.
pub fn bytes_to_mib(bytes: u64) -> u32 {
    let mib = bytes.div_ceil(MIB);
    u32::try_from(mib).unwrap_or(u32::MAX)
}

/// Peak local disk one job needs, as a multiple of its input size.
///
/// The source is deleted right after encoding wherever verification can work from
/// a recorded digest, which is what keeps most of these under 2x.
pub fn peak_reservation_ratio(recipe: Recipe, keep_source: bool) -> f64 {
    let base = match recipe {
        // input + output, then input dropped and the djxl rebuild written.
        Recipe::JxlFromJpeg | Recipe::JxlFromRaster | Recipe::JxlFromWebp => 1.8,
        Recipe::Flac | Recipe::FlacRecompress => 1.5,
        Recipe::TsRemux => 1.95,
        // Lossless video can grow, and framemd5 needs both sides present.
        Recipe::Ffv1 => 2.5,
        // VMAF decodes source and result together, so the source cannot be dropped.
        Recipe::Av1 => 1.65,
    };
    // The figures above assume the source is dropped as soon as it is no longer
    // needed. Keeping it for `--keep-originals` means it is still on disk while
    // the output and any verification rebuild exist, which is one more copy than
    // they account for. The video recipes already hold it, so they are already
    // counted.
    if keep_source && !holds_source(recipe) {
        base + 1.0
    } else {
        base
    }
}

/// Whether a recipe needs its source present through verification.
pub fn holds_source(recipe: Recipe) -> bool {
    matches!(recipe, Recipe::Av1 | Recipe::Ffv1)
}

pub fn reservation_mib(recipe: Recipe, input_bytes: u64, keep_source: bool) -> u32 {
    bytes_to_mib((input_bytes as f64 * peak_reservation_ratio(recipe, keep_source)).ceil() as u64)
}

/// A held slice of a budget. Dropping it returns the capacity.
#[derive(Debug)]
pub struct Lease {
    permit: OwnedSemaphorePermit,
    pub mib: u32,
}

impl Lease {
    /// Keeps the capacity spent after the lease goes out of scope.
    ///
    /// Uploaded bytes occupy the remote past the end of the job that wrote them:
    /// the original it replaces sits in the trash, still billed, until the trash
    /// is emptied. Holding the slice keeps the budget honest about that, and
    /// [`Governor::release_cloud`] is what hands it back.
    pub fn hold(self) {
        self.permit.forget();
    }
}

/// Cores granted to one encode, returned to the pool on drop.
#[derive(Debug)]
pub struct CpuLease {
    _permit: OwnedSemaphorePermit,
    cores: Vec<usize>,
    pool: Arc<Mutex<Vec<usize>>>,
}

impl CpuLease {
    /// Core ids to pin the encoder to.
    pub fn cores(&self) -> &[usize] {
        &self.cores
    }
}

impl Drop for CpuLease {
    fn drop(&mut self) {
        if let Ok(mut free) = self.pool.lock() {
            free.append(&mut self.cores);
        }
    }
}

/// A plain count-based slot, for network transfers and API calls.
#[derive(Debug)]
pub struct Slot(#[allow(dead_code)] OwnedSemaphorePermit);

#[derive(Debug)]
pub struct Governor {
    disk: Arc<Semaphore>,
    disk_capacity_mib: u32,
    cloud: Arc<Semaphore>,
    cloud_capacity_mib: u32,
    net: Arc<Semaphore>,
    api: Arc<Semaphore>,
    /// Cores video may take. Separate from the pool everything else draws on,
    /// because one semaphore cannot serve both: tokio hands out permits in
    /// request order, so a video waiting for fourteen of them takes the twelve
    /// that are free and holds them idle while every one-core job queues behind
    /// it. The setting meant to keep image work flowing did the opposite.
    cpu_video: Arc<Semaphore>,
    /// Cores reserved for everything that is not video.
    cpu_other: Arc<Semaphore>,
    video_cores: usize,
    free_cores: Arc<Mutex<Vec<usize>>>,
}

impl Governor {
    /// `cloud_free_bytes` is what the remote reports, before the configured safety
    /// margin is subtracted.
    pub fn new(cfg: &Config, cloud_free_bytes: u64) -> Result<Self> {
        // `resolve` already refuses a reserve that leaves video nothing.
        let video_cores = cfg.cpu_cores.saturating_sub(cfg.non_video_cores).max(1);
        let cloud_mib = bytes_to_mib(cloud_free_bytes).saturating_sub(cfg.cloud_reserve_mib);
        if cloud_mib == 0 {
            bail!(
                "no usable free space on the remote: {} free, {} MiB held back as reserve. \
                 Empty the trash with `cleanup --execute`, or lower cloud_reserve_gb.",
                humansize::format_size(cloud_free_bytes, humansize::DECIMAL),
                cfg.cloud_reserve_mib
            );
        }
        if cloud_mib < cfg.max_file_mib {
            bail!(
                "remote free space ({cloud_mib} MiB after reserve) is below max_file_gb \
                 ({} MiB), so the largest permitted file could never be uploaded. \
                 Reclaim space first with `cleanup --execute`.",
                cfg.max_file_mib
            );
        }

        Ok(Self {
            disk: Arc::new(Semaphore::new(cfg.staging_budget_mib as usize)),
            disk_capacity_mib: cfg.staging_budget_mib,
            cloud: Arc::new(Semaphore::new(cloud_mib as usize)),
            cloud_capacity_mib: cloud_mib,
            net: Arc::new(Semaphore::new(cfg.net_concurrency)),
            api: Arc::new(Semaphore::new(cfg.api_concurrency)),
            cpu_video: Arc::new(Semaphore::new(video_cores)),
            cpu_other: Arc::new(Semaphore::new(cfg.non_video_cores)),
            video_cores,
            free_cores: Arc::new(Mutex::new((0..cfg.cpu_cores).collect())),
        })
    }

    pub fn disk_capacity_mib(&self) -> u32 {
        self.disk_capacity_mib
    }

    pub fn cloud_capacity_mib(&self) -> u32 {
        self.cloud_capacity_mib
    }

    /// MiB currently unspent on the remote budget.
    pub fn cloud_available_mib(&self) -> u32 {
        u32::try_from(self.cloud.available_permits()).unwrap_or(u32::MAX)
    }

    /// Reserves local staging space for the whole life of a job.
    ///
    /// Fails rather than blocking forever when the request exceeds total capacity,
    /// which would otherwise be an indefinite hang on one oversized file.
    pub async fn disk(&self, mib: u32) -> Result<Lease> {
        if mib > self.disk_capacity_mib {
            bail!(
                "needs {mib} MiB of staging but the budget is only {} MiB",
                self.disk_capacity_mib
            );
        }
        let permit = self
            .disk
            .clone()
            .acquire_many_owned(mib.max(1))
            .await
            .expect("disk semaphore is never closed");
        Ok(Lease { permit, mib })
    }

    /// Reserves remote quota for an upload.
    ///
    /// Released only when the trash is emptied, because deleting the original moves
    /// it to the trash and Filen counts the trash against the quota.
    pub async fn cloud(&self, mib: u32) -> Result<Lease> {
        if mib > self.cloud_capacity_mib {
            bail!(
                "needs {mib} MiB of remote quota but only {} MiB is available",
                self.cloud_capacity_mib
            );
        }
        let permit = self
            .cloud
            .clone()
            .acquire_many_owned(mib.max(1))
            .await
            .expect("cloud semaphore is never closed");
        Ok(Lease { permit, mib })
    }

    /// Returns quota to the budget after the trash has actually been emptied.
    pub fn release_cloud(&self, mib: u32) {
        self.cloud.add_permits(mib as usize);
    }

    pub async fn network(&self) -> Slot {
        Slot(
            self.net
                .clone()
                .acquire_owned()
                .await
                .expect("net semaphore is never closed"),
        )
    }

    pub async fn api(&self) -> Slot {
        Slot(
            self.api
                .clone()
                .acquire_owned()
                .await
                .expect("api semaphore is never closed"),
        )
    }

    /// Grants cores for an encode.
    ///
    /// Video asks for everything except `non_video_cores`, so image and audio
    /// work keeps flowing through a long AV1 encode instead of the pipeline
    /// stalling on it. Everything else takes a single core.
    pub async fn cpu(&self, recipe: Recipe) -> CpuLease {
        let want = self.cores_for(recipe);
        let permit = if holds_source(recipe) {
            // Video draws only on its own pool, so a large request can never
            // queue in front of a small one.
            self.cpu_video
                .clone()
                .acquire_many_owned(want as u32)
                .await
                .expect("cpu semaphore is never closed")
        } else {
            self.one_core().await
        };

        let mut cores = Vec::with_capacity(want);
        if let Ok(mut free) = self.free_cores.lock() {
            for _ in 0..want {
                // The permit guarantees availability, so the pool cannot be short.
                if let Some(core) = free.pop() {
                    cores.push(core);
                }
            }
        }
        CpuLease {
            _permit: permit,
            cores,
            pool: Arc::clone(&self.free_cores),
        }
    }

    /// Takes one core for a job that only needs one.
    ///
    /// Its own pool first. Failing that it will use an idle video core rather
    /// than leave one doing nothing — but only if one is free right now, never
    /// by queueing, because queueing on that pool is what would put it in front
    /// of a video encode.
    async fn one_core(&self) -> OwnedSemaphorePermit {
        if let Ok(permit) = self.cpu_other.clone().try_acquire_owned() {
            return permit;
        }
        if let Ok(permit) = self.cpu_video.clone().try_acquire_owned() {
            return permit;
        }
        self.cpu_other
            .clone()
            .acquire_owned()
            .await
            .expect("cpu semaphore is never closed")
    }

    fn cores_for(&self, recipe: Recipe) -> usize {
        match recipe {
            _ if holds_source(recipe) => self.video_cores,
            _ => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, FileConfig, Overrides};

    const GB: u64 = 1024 * 1024 * 1024;

    fn config(budget_gb: u64, max_file_gb: u64, cpus: usize) -> Config {
        Config::resolve(
            FileConfig {
                staging_budget_gb: Some(budget_gb),
                max_file_gb: Some(max_file_gb),
                cpu_cores: Some(cpus),
                non_video_cores: Some(2),
                cloud_reserve_gb: Some(0),
                ..Default::default()
            },
            Overrides::default(),
            1000 * GB,
        )
        .unwrap()
    }

    fn governor(budget_gb: u64, max_file_gb: u64, cpus: usize, cloud_gb: u64) -> Governor {
        Governor::new(&config(budget_gb, max_file_gb, cpus), cloud_gb * GB).unwrap()
    }

    #[test]
    fn converts_bytes_to_whole_mib() {
        assert_eq!(bytes_to_mib(0), 0);
        assert_eq!(bytes_to_mib(1), 1);
        assert_eq!(bytes_to_mib(MIB), 1);
        assert_eq!(bytes_to_mib(MIB + 1), 2);
        assert_eq!(bytes_to_mib(100 * MIB), 100);
    }

    /// The reason permits are MiB and not bytes: an 8 GiB file does not fit in u32
    /// bytes, but its MiB count fits comfortably.
    #[test]
    fn large_files_fit_in_u32_permits() {
        let eight_gib = 8 * GB;
        assert!(u32::try_from(eight_gib).is_err());
        assert_eq!(bytes_to_mib(eight_gib), 8192);
        assert_eq!(reservation_mib(Recipe::JxlFromJpeg, eight_gib, false), 14746);
    }

    #[test]
    fn reservations_follow_the_recipe() {
        let one_gib = GB;
        assert_eq!(reservation_mib(Recipe::JxlFromJpeg, one_gib, false), 1844);
        assert_eq!(reservation_mib(Recipe::Flac, one_gib, false), 1536);
        assert_eq!(reservation_mib(Recipe::Ffv1, one_gib, false), 2560);
        assert_eq!(reservation_mib(Recipe::Av1, one_gib, false), 1690);
    }

    #[tokio::test]
    async fn disk_leases_return_capacity_when_dropped() {
        let g = governor(10, 5, 4, 100);
        let before = g.disk.available_permits();
        {
            let _lease = g.disk(1024).await.unwrap();
            assert_eq!(g.disk.available_permits(), before - 1024);
        }
        assert_eq!(g.disk.available_permits(), before);
    }

    /// Blocking forever on a request that can never be satisfied would hang the
    /// whole run on one oversized file.
    #[tokio::test]
    async fn an_impossible_disk_request_fails_instead_of_hanging() {
        let g = governor(1, 1, 4, 100);
        let err = g.disk(99_999).await.unwrap_err();
        assert!(err.to_string().contains("budget is only"), "{err}");
    }

    #[tokio::test]
    async fn disk_pressure_makes_later_jobs_wait() {
        let g = Arc::new(governor(1, 1, 4, 100));
        let held = g.disk(1000).await.unwrap(); // 1 GiB budget is 1024 MiB
        let g2 = Arc::clone(&g);
        let waiter = tokio::spawn(async move { g2.disk(100).await.map(|l| l.mib) });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!waiter.is_finished(), "should still be waiting for space");

        drop(held);
        assert_eq!(waiter.await.unwrap().unwrap(), 100);
    }

    /// Local disk and remote quota are separate limits with different sizes; a
    /// generous local budget must not imply room to upload.
    #[tokio::test]
    async fn cloud_quota_is_independent_of_local_disk() {
        let g = governor(50, 1, 4, 3); // 50 GiB local, 3 GiB remote
        assert_eq!(g.disk_capacity_mib(), 50 * 1024);
        assert_eq!(g.cloud_capacity_mib(), 3 * 1024);

        let err = g.cloud(4 * 1024).await.unwrap_err();
        assert!(err.to_string().contains("remote quota"), "{err}");
    }

    /// With the default trash policy nothing returns quota, so the budget drains
    /// and uploads eventually stop. That is the safe outcome, and it must be the
    /// observed one rather than an overwrite.
    #[tokio::test]
    async fn cloud_budget_drains_when_nothing_is_reclaimed() {
        let g = governor(50, 1, 4, 2); // 2 GiB of remote headroom
        let _a = g.cloud(1024).await.unwrap();
        let _b = g.cloud(1024).await.unwrap();
        assert_eq!(g.cloud_available_mib(), 0);

        let pending = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            g.cloud(1),
        )
        .await;
        assert!(pending.is_err(), "further uploads must wait, not proceed");
    }

    #[tokio::test]
    async fn emptying_the_trash_returns_quota() {
        let g = governor(50, 1, 4, 2);
        let lease = g.cloud(2048).await.unwrap();
        assert_eq!(g.cloud_available_mib(), 0);
        // The upload holds its slice until the original is actually purged.
        drop(lease);
        g.release_cloud(2048);
        assert!(g.cloud_available_mib() >= 2048);
    }

    #[test]
    fn a_full_remote_is_refused_up_front() {
        let cfg = config(50, 10, 4);
        let err = Governor::new(&cfg, 1024).unwrap_err();
        assert!(err.to_string().contains("below max_file_gb"), "{err}");
    }

    #[test]
    fn an_exhausted_remote_is_refused_up_front() {
        let mut cfg = config(50, 10, 4);
        cfg.cloud_reserve_mib = 5 * 1024;
        let err = Governor::new(&cfg, GB).unwrap_err();
        assert!(err.to_string().contains("no usable free space"), "{err}");
    }

    /// Video takes all but the reserved cores so image work keeps flowing; the
    /// pinned set must match what was granted.
    #[tokio::test]
    async fn video_leaves_cores_for_everything_else() {
        let g = governor(50, 10, 8, 100);
        let video = g.cpu(Recipe::Av1).await;
        assert_eq!(video.cores().len(), 6, "8 cores minus 2 reserved");

        let image = g.cpu(Recipe::JxlFromJpeg).await;
        assert_eq!(image.cores().len(), 1);

        // No core is handed to two jobs at once.
        let mut all: Vec<_> = video.cores().iter().chain(image.cores()).copied().collect();
        let total = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), total);
    }

    #[tokio::test]
    async fn cores_return_to_the_pool() {
        let g = governor(50, 10, 4, 100);
        {
            let lease = g.cpu(Recipe::Av1).await;
            assert_eq!(lease.cores().len(), 2);
            assert_eq!(g.free_cores.lock().unwrap().len(), 2);
        }
        assert_eq!(g.free_cores.lock().unwrap().len(), 4);
    }

    /// The smallest configuration the validator permits still leaves video a core.
    #[tokio::test]
    async fn video_always_gets_at_least_one_core() {
        let g = governor(50, 10, 3, 100);
        assert_eq!(g.cpu(Recipe::Av1).await.cores().len(), 1);
    }

    /// One semaphore cannot serve both sizes of request. Tokio hands out permits
    /// in request order, so a video waiting for most of the pool takes what is
    /// free and holds it idle until the rest arrives, with every one-core job
    /// queued behind it — the setting meant to keep image work flowing doing the
    /// exact opposite. Separate pools are what make it true.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_waiting_video_does_not_block_image_work() {
        // 8 cores, 2 of them reserved, so video gets 6.
        let g = Arc::new(governor(100, 10, 8, 500));

        // A video takes all six of its cores, so its pool is empty.
        let hog = g.cpu(Recipe::Av1).await;
        assert_eq!(hog.cores().len(), 6);

        // A second video queues, which is where the old shared pool would start
        // swallowing permits an image job needed.
        let waiting = tokio::spawn({
            let g = Arc::clone(&g);
            async move { g.cpu(Recipe::Av1).await }
        });
        tokio::task::yield_now().await;

        // The reserved cores are still there for image work.
        for _ in 0..2 {
            let lease = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                g.cpu(Recipe::JxlFromJpeg),
            )
            .await
            .expect("an image must not queue behind a video");
            assert_eq!(lease.cores().len(), 1);
        }

        waiting.abort();
    }

    /// With no video running its cores should not sit idle, so a one-core job
    /// borrows them rather than queueing on its own small pool.
    #[tokio::test]
    async fn image_work_uses_idle_video_cores() {
        let g = governor(100, 10, 8, 500);

        let mut held = Vec::new();
        for _ in 0..8 {
            held.push(
                tokio::time::timeout(
                    std::time::Duration::from_millis(500),
                    g.cpu(Recipe::JxlFromJpeg),
                )
                .await
                .expect("all eight cores should be reachable when no video wants them"),
            );
        }
    }

    /// Keeping the original means it is still on disk while the output and any
    /// verification rebuild exist. A reservation that assumed it had been
    /// dropped would let the staging budget be overrun by exactly one copy of
    /// every file in flight.
    #[test]
    fn keeping_the_original_reserves_room_for_it() {
        let gib = 1024 * MIB;
        assert_eq!(reservation_mib(Recipe::JxlFromJpeg, gib, false), 1844);
        assert_eq!(reservation_mib(Recipe::JxlFromJpeg, gib, true), 2868);

        // The video recipes already hold their source through verification, so
        // they must not be charged for it twice.
        for recipe in [Recipe::Av1, Recipe::Ffv1] {
            assert_eq!(
                reservation_mib(recipe, gib, true),
                reservation_mib(recipe, gib, false),
                "{recipe:?} already holds its source"
            );
            assert!(holds_source(recipe));
        }
        assert!(!holds_source(Recipe::JxlFromJpeg));
    }

    #[tokio::test]
    async fn network_and_api_slots_are_bounded() {
        let g = governor(50, 10, 4, 100);
        let mut held = Vec::new();
        for _ in 0..g.net.available_permits() {
            held.push(g.network().await);
        }
        let blocked =
            tokio::time::timeout(std::time::Duration::from_millis(50), g.network()).await;
        assert!(blocked.is_err(), "network slots should be exhausted");
    }
}
