//! The module scheduler (MOD-03) and the [`Timer`] it sleeps on.
//!
//! What a module scheduled lives in its [`Schedules`]: name → period, at
//! most [`MAX_SCHEDULES`] names. The `schedule` import changes it in place
//! and wakes the scheduler, one tokio task per module. For every name the
//! task keeps exactly one pending sleep on the [`Timer`]; when it ends the
//! task calls `tick(name)` and waits for it to return before that name's
//! next period starts, so calls of one name never overlap and a slow tick
//! delays its next one instead of piling calls up (missed periods are not
//! caught up). Scheduling a name again replaces its pending sleep (the old
//! one is aborted); `every-secs = 0` stops it. Tests drive it with a fake
//! timer.

use core::pin::Pin;
use core::time::Duration;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use futures_util::future::{AbortHandle, Abortable, abortable};
use futures_util::stream::{FuturesUnordered, StreamExt as _};
use liftoff_core::domain::ModuleName;
use liftoff_core::module::CoreCalls;
use liftoff_core::ports::ModuleStore;
use tokio::sync::Notify;

use crate::limits::{MAX_SCHEDULE_NAME_BYTES, MAX_SCHEDULES, MIN_PERIOD_SECS};
use crate::log::ModuleLog;
use crate::module::{Slot, calling};

/// Sleeps on a clock: tokio's in the daemon, a fake one in tests.
pub trait Timer: Send + Sync + 'static {
    /// Completes once `period` has passed on this timer's clock.
    fn sleep(&self, period: Duration) -> impl Future<Output = ()> + Send;
}

/// [`Timer`] on tokio's clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioTimer;

impl Timer for TokioTimer {
    fn sleep(&self, period: Duration) -> impl Future<Output = ()> + Send {
        tokio::time::sleep(period)
    }
}

/// The names a module scheduled and which of them changed since the
/// scheduler last looked.
#[derive(Debug, Default)]
struct Table {
    periods: BTreeMap<String, Duration>,
    changed: BTreeSet<String>,
}

/// What a module scheduled, shared by its `schedule` import and its
/// scheduler task.
#[derive(Debug, Default)]
pub(crate) struct Schedules {
    table: Mutex<Table>,
    wake: Notify,
}

impl Schedules {
    fn table(&self) -> std::sync::MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `schedule(name, every-secs)` of `module`: stops the name for `0`,
    /// sets its period otherwise. A name longer than
    /// [`MAX_SCHEDULE_NAME_BYTES`], a period shorter than
    /// [`MIN_PERIOD_SECS`] and a new name beyond [`MAX_SCHEDULES`] are
    /// ignored and logged (the WIT `schedule` has no error).
    pub(crate) fn request(&self, module: &ModuleName, name: String, every_secs: u32) {
        let refused = if name.len() > MAX_SCHEDULE_NAME_BYTES {
            Some("the name is too long")
        } else if every_secs != 0 && every_secs < MIN_PERIOD_SECS {
            Some("the period is too short")
        } else {
            let mut table = self.table();
            if every_secs == 0 {
                table.periods.remove(&name);
                table.changed.insert(name);
                None
            } else if table.periods.len() >= MAX_SCHEDULES && !table.periods.contains_key(&name) {
                Some("the module has too many scheduled names")
            } else {
                let period = Duration::from_secs(u64::from(every_secs));
                table.periods.insert(name.clone(), period);
                table.changed.insert(name);
                None
            }
        };
        match refused {
            Some(why) => tracing::warn!(
                %module,
                every_secs,
                min_secs = MIN_PERIOD_SECS,
                max_names = MAX_SCHEDULES,
                "schedule ignored: {why}"
            ),
            None => self.wake.notify_one(),
        }
    }

    /// The names changed since the last call, with their period now
    /// (`None`: stopped).
    fn take_changes(&self) -> Vec<(String, Option<Duration>)> {
        let mut table = self.table();
        let changed = core::mem::take(&mut table.changed);
        let periods = changed
            .into_iter()
            .map(|name| {
                let period = table.periods.get(&name).copied();
                (name, period)
            })
            .collect();
        drop(table);
        periods
    }

    /// The period of `name` now, if it is scheduled.
    fn period(&self, name: &str) -> Option<Duration> {
        self.table().periods.get(name).copied()
    }
}

/// A pending period: ends with its name, or is aborted.
type Sleep = Abortable<Pin<Box<dyn Future<Output = String> + Send>>>;

/// The one pending sleep of every scheduled name.
struct Sleeps<T> {
    timer: Arc<T>,
    handles: BTreeMap<String, AbortHandle>,
    pending: FuturesUnordered<Sleep>,
}

impl<T: Timer> Sleeps<T> {
    /// Starts a sleep of `period` for `name`, aborting the one it had.
    fn start(&mut self, name: String, period: Duration) {
        let timer = Arc::clone(&self.timer);
        let ends = name.clone();
        let sleep: Pin<Box<dyn Future<Output = String> + Send>> = Box::pin(async move {
            timer.sleep(period).await;
            ends
        });
        let (sleep, handle) = abortable(sleep);
        if let Some(replaced) = self.handles.insert(name, handle) {
            replaced.abort();
        }
        self.pending.push(sleep);
    }

    /// Aborts the sleep of `name`, if it has one.
    fn stop(&mut self, name: &str) {
        if let Some(stopped) = self.handles.remove(name) {
            stopped.abort();
        }
    }
}

/// The scheduler task of `module`: runs until the module is dropped (the
/// task is aborted).
pub(crate) async fn run<T: Timer, S: ModuleStore + 'static, L: ModuleLog, C: CoreCalls>(
    module: ModuleName,
    timer: Arc<T>,
    schedules: Arc<Schedules>,
    slot: Arc<tokio::sync::Mutex<Slot<S, L, C>>>,
) {
    let mut sleeps = Sleeps {
        timer,
        handles: BTreeMap::new(),
        pending: FuturesUnordered::new(),
    };
    loop {
        tokio::select! {
            () = schedules.wake.notified() => {
                for (name, period) in schedules.take_changes() {
                    match period {
                        Some(period) => sleeps.start(name, period),
                        None => sleeps.stop(&name),
                    }
                }
            }
            Some(ended) = sleeps.pending.next(), if !sleeps.pending.is_empty() => {
                // An aborted sleep was replaced or stopped.
                let Ok(name) = ended else {
                    continue;
                };
                sleeps.handles.remove(&name);
                if schedules.period(&name).is_none() {
                    continue;
                }
                let ticked =
                    calling(&module, async { slot.lock().await.tick(&name).await })
                        .await;
                if let Err(error) = ticked {
                    tracing::warn!(%module, tick = %name, %error, "a scheduled tick failed");
                }
                // The next period starts once this tick returned; a
                // reschedule during the tick replaces it right after.
                if let Some(period) = schedules.period(&name) {
                    sleeps.start(name, period);
                }
            }
        }
    }
}
