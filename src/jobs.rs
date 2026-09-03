//! Multi-printer job queue and automatic routing.
//!
//! This module is deliberately independent of the UI and transport layers. It
//! contains only serializable state and synchronous state transitions, making
//! it suitable for native builds, WebAssembly, persistence, and deterministic
//! tests.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub type JobId = u64;
pub type PrinterId = String;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JobStatus {
    Queued,
    Running,
    Done,
    Error,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum PrinterStatus {
    Idle,
    Busy { job_id: JobId },
    Offline,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Printer {
    pub id: PrinterId,
    pub name: String,
    pub status: PrinterStatus,
    /// Features understood by the printer, such as `print`, `cut`, or a media
    /// size. Jobs may require any number of these values.
    #[serde(default)]
    pub capabilities: BTreeSet<String>,
}

impl Printer {
    pub fn new(id: impl Into<PrinterId>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            status: PrinterStatus::Idle,
            capabilities: BTreeSet::new(),
        }
    }

    pub fn with_capabilities<I, S>(mut self, capabilities: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.capabilities = capabilities.into_iter().map(Into::into).collect();
        self
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSpec {
    pub name: String,
    /// Every listed capability must be present on the selected printer.
    #[serde(default)]
    pub required_capabilities: BTreeSet<String>,
    /// When non-empty, restrict routing to these printer IDs.
    #[serde(default)]
    pub eligible_printers: BTreeSet<PrinterId>,
}

impl JobSpec {
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    pub fn requiring<I, S>(mut self, capabilities: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.required_capabilities = capabilities.into_iter().map(Into::into).collect();
        self
    }

    pub fn restricted_to<I, S>(mut self, printers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<PrinterId>,
    {
        self.eligible_printers = printers.into_iter().map(Into::into).collect();
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub id: JobId,
    pub spec: JobSpec,
    pub status: JobStatus,
    /// Integer percentage avoids NaN and remains convenient for UI progress
    /// bars after JSON round trips.
    pub progress_percent: u8,
    pub assigned_printer: Option<PrinterId>,
    pub error: Option<String>,
    /// Number of times this job has been assigned to a printer.
    pub attempts: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub job_id: JobId,
    pub printer_id: PrinterId,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum QueueError {
    #[error("printer ID cannot be empty")]
    EmptyPrinterId,
    #[error("printer {0:?} already exists")]
    DuplicatePrinter(PrinterId),
    #[error("a newly added printer cannot already be busy")]
    BusyPrinterRegistration,
    #[error("printer {0:?} was not found")]
    UnknownPrinter(PrinterId),
    #[error("job {0} was not found")]
    UnknownJob(JobId),
    #[error("progress must be between 0 and 100, got {0}")]
    InvalidProgress(u8),
    #[error("job {job_id} cannot transition from {status:?} via {operation}")]
    InvalidJobTransition {
        job_id: JobId,
        status: JobStatus,
        operation: &'static str,
    },
    #[error("invalid persisted queue state: {0}")]
    InvalidState(String),
}

/// Serializable queue state. Printer insertion order is retained to make
/// routing predictable when multiple printers are equally suitable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "JobQueueData")]
pub struct JobQueue {
    next_job_id: JobId,
    printers: Vec<Printer>,
    jobs: BTreeMap<JobId, Job>,
    queued: VecDeque<JobId>,
}

#[derive(Deserialize)]
struct JobQueueData {
    next_job_id: JobId,
    printers: Vec<Printer>,
    jobs: BTreeMap<JobId, Job>,
    queued: VecDeque<JobId>,
}

impl TryFrom<JobQueueData> for JobQueue {
    type Error = QueueError;

    fn try_from(value: JobQueueData) -> Result<Self, Self::Error> {
        let queue = Self {
            next_job_id: value.next_job_id,
            printers: value.printers,
            jobs: value.jobs,
            queued: value.queued,
        };
        queue.validate()?;
        Ok(queue)
    }
}

impl Default for JobQueue {
    fn default() -> Self {
        Self {
            next_job_id: 1,
            printers: Vec::new(),
            jobs: BTreeMap::new(),
            queued: VecDeque::new(),
        }
    }
}

impl JobQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn printers(&self) -> &[Printer] {
        &self.printers
    }

    pub fn jobs(&self) -> impl Iterator<Item = &Job> {
        self.jobs.values()
    }

    pub fn queued_job_ids(&self) -> impl Iterator<Item = JobId> + '_ {
        self.queued.iter().copied()
    }

    pub fn printer(&self, id: &str) -> Option<&Printer> {
        self.printers.iter().find(|printer| printer.id == id)
    }

    pub fn job(&self, id: JobId) -> Option<&Job> {
        self.jobs.get(&id)
    }

    pub fn add_printer(&mut self, printer: Printer) -> Result<(), QueueError> {
        if printer.id.trim().is_empty() {
            return Err(QueueError::EmptyPrinterId);
        }
        if self.printer(&printer.id).is_some() {
            return Err(QueueError::DuplicatePrinter(printer.id));
        }
        if matches!(printer.status, PrinterStatus::Busy { .. }) {
            return Err(QueueError::BusyPrinterRegistration);
        }
        self.printers.push(printer);
        Ok(())
    }

    pub fn enqueue(&mut self, spec: JobSpec) -> JobId {
        let id = self.next_job_id;
        self.next_job_id = self
            .next_job_id
            .checked_add(1)
            .expect("job ID space exhausted");
        self.jobs.insert(
            id,
            Job {
                id,
                spec,
                status: JobStatus::Queued,
                progress_percent: 0,
                assigned_printer: None,
                error: None,
                attempts: 0,
            },
        );
        self.queued.push_back(id);
        id
    }

    /// Assign the oldest routable job to the first eligible idle printer.
    /// Jobs that currently have no eligible printer do not block later jobs.
    pub fn route_next(&mut self) -> Option<Route> {
        let (queue_index, printer_index) =
            self.queued.iter().enumerate().find_map(|(index, id)| {
                let job = self.jobs.get(id)?;
                if job.status != JobStatus::Queued {
                    return None;
                }
                self.printers
                    .iter()
                    .position(|printer| Self::printer_can_run(printer, job))
                    .map(|printer_index| (index, printer_index))
            })?;

        let job_id = self.queued.remove(queue_index)?;
        let printer_id = self.printers[printer_index].id.clone();
        self.printers[printer_index].status = PrinterStatus::Busy { job_id };

        let job = self
            .jobs
            .get_mut(&job_id)
            .expect("queued job ID must reference an existing job");
        job.status = JobStatus::Running;
        job.assigned_printer = Some(printer_id.clone());
        job.error = None;
        job.attempts = job.attempts.saturating_add(1);

        Some(Route { job_id, printer_id })
    }

    /// Fill as many idle printers as possible, returning assignments in queue
    /// order. Augmenting-path matching avoids wasting a specialized printer on
    /// a flexible job when another idle printer can run that job.
    pub fn route_available(&mut self) -> Vec<Route> {
        let queue_indices: Vec<usize> = self
            .queued
            .iter()
            .enumerate()
            .filter_map(|(index, id)| {
                (self.jobs.get(id)?.status == JobStatus::Queued).then_some(index)
            })
            .collect();
        let printer_indices: Vec<usize> = self
            .printers
            .iter()
            .enumerate()
            .filter_map(|(index, printer)| {
                matches!(printer.status, PrinterStatus::Idle).then_some(index)
            })
            .collect();
        let mut printer_matches = vec![None; printer_indices.len()];
        for &queue_index in &queue_indices {
            let mut visited = vec![false; printer_indices.len()];
            self.augment_match(
                queue_index,
                &printer_indices,
                &mut printer_matches,
                &mut visited,
            );
        }

        let mut assignments: Vec<(usize, usize)> = printer_matches
            .into_iter()
            .enumerate()
            .filter_map(|(slot, queue_index)| {
                queue_index.map(|queue_index| (queue_index, printer_indices[slot]))
            })
            .collect();
        assignments.sort_unstable_by_key(|(queue_index, _)| *queue_index);
        let assigned_ids: BTreeSet<JobId> = assignments
            .iter()
            .map(|(queue_index, _)| self.queued[*queue_index])
            .collect();
        let routes = assignments
            .into_iter()
            .map(|(queue_index, printer_index)| {
                let job_id = self.queued[queue_index];
                self.assign(job_id, printer_index)
            })
            .collect();
        self.queued.retain(|id| !assigned_ids.contains(id));
        routes
    }

    /// Validate all correlated IDs and states. Deserialization calls this
    /// automatically, preventing malformed persisted data from entering use.
    pub fn validate(&self) -> Result<(), QueueError> {
        let mut printer_ids = BTreeSet::new();
        for printer in &self.printers {
            if printer.id.trim().is_empty() || !printer_ids.insert(printer.id.as_str()) {
                return Err(QueueError::InvalidState(format!(
                    "invalid or duplicate printer ID {:?}",
                    printer.id
                )));
            }
            if let PrinterStatus::Busy { job_id } = printer.status {
                let job = self.jobs.get(&job_id).ok_or_else(|| {
                    QueueError::InvalidState(format!(
                        "printer {:?} references missing job {job_id}",
                        printer.id
                    ))
                })?;
                if job.status != JobStatus::Running
                    || job.assigned_printer.as_deref() != Some(printer.id.as_str())
                {
                    return Err(QueueError::InvalidState(format!(
                        "printer {:?} and job {job_id} disagree",
                        printer.id
                    )));
                }
            }
        }

        let mut queued_ids = BTreeSet::new();
        for job_id in &self.queued {
            if !queued_ids.insert(*job_id) {
                return Err(QueueError::InvalidState(format!(
                    "job {job_id} occurs more than once in queue order"
                )));
            }
            if self.jobs.get(job_id).map(|job| job.status) != Some(JobStatus::Queued) {
                return Err(QueueError::InvalidState(format!(
                    "queue entry {job_id} is missing or not queued"
                )));
            }
        }
        for (key, job) in &self.jobs {
            if job.progress_percent > 100 {
                return Err(QueueError::InvalidState(format!(
                    "job {key} has invalid progress {}",
                    job.progress_percent
                )));
            }
            if *key != job.id {
                return Err(QueueError::InvalidState(format!(
                    "job key {key} does not match embedded ID {}",
                    job.id
                )));
            }
            if job.status == JobStatus::Queued && !queued_ids.contains(key) {
                return Err(QueueError::InvalidState(format!(
                    "queued job {key} is absent from queue order"
                )));
            }
            if job.status == JobStatus::Running {
                let printer_id = job.assigned_printer.as_deref().ok_or_else(|| {
                    QueueError::InvalidState(format!("running job {key} has no printer"))
                })?;
                if !matches!(
                    self.printer(printer_id).map(|printer| &printer.status),
                    Some(PrinterStatus::Busy { job_id }) if job_id == key
                ) {
                    return Err(QueueError::InvalidState(format!(
                        "running job {key} is not owned by printer {printer_id:?}"
                    )));
                }
            }
        }
        if self.next_job_id == 0
            || self
                .jobs
                .last_key_value()
                .is_some_and(|(max_id, _)| self.next_job_id <= *max_id)
        {
            return Err(QueueError::InvalidState(format!(
                "next job ID {} can collide",
                self.next_job_id
            )));
        }
        Ok(())
    }

    pub fn update_progress(&mut self, job_id: JobId, percent: u8) -> Result<(), QueueError> {
        if percent > 100 {
            return Err(QueueError::InvalidProgress(percent));
        }
        let job = self.job_mut_for(job_id, JobStatus::Running, "update progress")?;
        job.progress_percent = percent;
        Ok(())
    }

    pub fn complete(&mut self, job_id: JobId) -> Result<(), QueueError> {
        self.job_mut_for(job_id, JobStatus::Running, "complete")?;
        self.release_printer(job_id);
        let job = self.jobs.get_mut(&job_id).expect("job was checked above");
        job.status = JobStatus::Done;
        job.progress_percent = 100;
        job.error = None;
        Ok(())
    }

    pub fn fail(&mut self, job_id: JobId, error: impl Into<String>) -> Result<(), QueueError> {
        self.job_mut_for(job_id, JobStatus::Running, "fail")?;
        self.release_printer(job_id);
        let job = self.jobs.get_mut(&job_id).expect("job was checked above");
        job.status = JobStatus::Error;
        job.error = Some(error.into());
        Ok(())
    }

    /// Cancel a queued or running job. Cancelling a running job immediately
    /// makes its printer available for another route.
    pub fn cancel(&mut self, job_id: JobId) -> Result<(), QueueError> {
        let status = self
            .jobs
            .get(&job_id)
            .ok_or(QueueError::UnknownJob(job_id))?
            .status;
        match status {
            JobStatus::Queued => self.queued.retain(|id| *id != job_id),
            JobStatus::Running => self.release_printer(job_id),
            _ => return Err(Self::transition_error(job_id, status, "cancel")),
        }
        let job = self.jobs.get_mut(&job_id).expect("job was checked above");
        job.status = JobStatus::Cancelled;
        job.error = None;
        Ok(())
    }

    /// Requeue a failed or cancelled job at the back of the queue.
    pub fn retry(&mut self, job_id: JobId) -> Result<(), QueueError> {
        let job = self
            .jobs
            .get_mut(&job_id)
            .ok_or(QueueError::UnknownJob(job_id))?;
        if !matches!(job.status, JobStatus::Error | JobStatus::Cancelled) {
            return Err(Self::transition_error(job_id, job.status, "retry"));
        }
        job.status = JobStatus::Queued;
        job.progress_percent = 0;
        job.assigned_printer = None;
        job.error = None;
        self.queued.push_back(job_id);
        Ok(())
    }

    /// Mark an idle printer offline. If it was working, its job becomes an
    /// error so the job can be retried on another eligible printer.
    pub fn set_printer_offline(
        &mut self,
        printer_id: &str,
        reason: impl Into<String>,
    ) -> Result<Option<JobId>, QueueError> {
        let index = self
            .printers
            .iter()
            .position(|printer| printer.id == printer_id)
            .ok_or_else(|| QueueError::UnknownPrinter(printer_id.to_owned()))?;
        let running_job = match self.printers[index].status {
            PrinterStatus::Busy { job_id } => Some(job_id),
            _ => None,
        };
        self.printers[index].status = PrinterStatus::Offline;
        if let Some(job_id) = running_job {
            let job = self
                .jobs
                .get_mut(&job_id)
                .expect("busy printer must reference an existing job");
            job.status = JobStatus::Error;
            job.error = Some(reason.into());
        }
        Ok(running_job)
    }

    pub fn set_printer_online(&mut self, printer_id: &str) -> Result<(), QueueError> {
        let printer = self
            .printers
            .iter_mut()
            .find(|printer| printer.id == printer_id)
            .ok_or_else(|| QueueError::UnknownPrinter(printer_id.to_owned()))?;
        if matches!(printer.status, PrinterStatus::Offline) {
            printer.status = PrinterStatus::Idle;
        }
        Ok(())
    }

    fn printer_can_run(printer: &Printer, job: &Job) -> bool {
        matches!(printer.status, PrinterStatus::Idle)
            && (job.spec.eligible_printers.is_empty()
                || job.spec.eligible_printers.contains(&printer.id))
            && job
                .spec
                .required_capabilities
                .is_subset(&printer.capabilities)
    }

    fn augment_match(
        &self,
        queue_index: usize,
        printer_indices: &[usize],
        printer_matches: &mut [Option<usize>],
        visited: &mut [bool],
    ) -> bool {
        let Some(job) = self
            .queued
            .get(queue_index)
            .and_then(|job_id| self.jobs.get(job_id))
        else {
            return false;
        };
        // Prefer a free eligible printer before disturbing an earlier match.
        for (slot, &printer_index) in printer_indices.iter().enumerate() {
            if !visited[slot]
                && printer_matches[slot].is_none()
                && Self::printer_can_run(&self.printers[printer_index], job)
            {
                visited[slot] = true;
                printer_matches[slot] = Some(queue_index);
                return true;
            }
        }
        // Otherwise move an earlier flexible job along an augmenting path.
        for (slot, &printer_index) in printer_indices.iter().enumerate() {
            if visited[slot] || !Self::printer_can_run(&self.printers[printer_index], job) {
                continue;
            }
            let Some(other_queue_index) = printer_matches[slot] else {
                continue;
            };
            visited[slot] = true;
            if self.augment_match(other_queue_index, printer_indices, printer_matches, visited) {
                printer_matches[slot] = Some(queue_index);
                return true;
            }
        }
        false
    }

    fn assign(&mut self, job_id: JobId, printer_index: usize) -> Route {
        let printer_id = self.printers[printer_index].id.clone();
        self.printers[printer_index].status = PrinterStatus::Busy { job_id };
        let job = self
            .jobs
            .get_mut(&job_id)
            .expect("validated queued job must exist");
        job.status = JobStatus::Running;
        job.assigned_printer = Some(printer_id.clone());
        job.error = None;
        job.attempts = job.attempts.saturating_add(1);
        Route { job_id, printer_id }
    }

    fn job_mut_for(
        &mut self,
        job_id: JobId,
        expected: JobStatus,
        operation: &'static str,
    ) -> Result<&mut Job, QueueError> {
        let job = self
            .jobs
            .get_mut(&job_id)
            .ok_or(QueueError::UnknownJob(job_id))?;
        if job.status != expected {
            return Err(Self::transition_error(job_id, job.status, operation));
        }
        Ok(job)
    }

    fn transition_error(job_id: JobId, status: JobStatus, operation: &'static str) -> QueueError {
        QueueError::InvalidJobTransition {
            job_id,
            status,
            operation,
        }
    }

    fn release_printer(&mut self, job_id: JobId) {
        if let Some(printer) = self.printers.iter_mut().find(|printer| {
            matches!(printer.status, PrinterStatus::Busy { job_id: running } if running == job_id)
        }) {
            printer.status = PrinterStatus::Idle;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn printer(id: &str, capabilities: &[&str]) -> Printer {
        Printer::new(id, format!("Printer {id}")).with_capabilities(capabilities.iter().copied())
    }

    #[test]
    fn routes_fifo_across_multiple_printers() {
        let mut queue = JobQueue::new();
        queue.add_printer(printer("a", &["print", "cut"])).unwrap();
        queue.add_printer(printer("b", &["print", "cut"])).unwrap();
        let first = queue.enqueue(JobSpec::named("one"));
        let second = queue.enqueue(JobSpec::named("two"));
        let third = queue.enqueue(JobSpec::named("three"));

        assert_eq!(
            queue.route_available(),
            vec![
                Route {
                    job_id: first,
                    printer_id: "a".into()
                },
                Route {
                    job_id: second,
                    printer_id: "b".into()
                }
            ]
        );
        assert_eq!(queue.queued_job_ids().collect::<Vec<_>>(), vec![third]);
        assert_eq!(queue.job(first).unwrap().status, JobStatus::Running);
        assert_eq!(queue.job(second).unwrap().attempts, 1);
    }

    #[test]
    fn batch_routing_maximizes_assignments_with_specialized_printers() {
        let mut queue = JobQueue::new();
        queue.add_printer(printer("special", &["cut"])).unwrap();
        queue.add_printer(printer("general", &[])).unwrap();
        let flexible = queue.enqueue(JobSpec::named("flexible"));
        let constrained = queue.enqueue(JobSpec::named("constrained").requiring(["cut"]));

        assert_eq!(
            queue.route_available(),
            vec![
                Route {
                    job_id: flexible,
                    printer_id: "general".into(),
                },
                Route {
                    job_id: constrained,
                    printer_id: "special".into(),
                },
            ]
        );
    }

    #[test]
    fn skips_temporarily_unroutable_jobs_without_losing_order() {
        let mut queue = JobQueue::new();
        queue.add_printer(printer("print", &["print"])).unwrap();
        let cut = queue.enqueue(JobSpec::named("cut").requiring(["cut"]));
        let print = queue.enqueue(JobSpec::named("print").requiring(["print"]));

        assert_eq!(queue.route_next().unwrap().job_id, print);
        assert_eq!(queue.queued_job_ids().collect::<Vec<_>>(), vec![cut]);
    }

    #[test]
    fn capability_and_printer_restrictions_are_both_applied() {
        let mut queue = JobQueue::new();
        queue.add_printer(printer("basic", &["print"])).unwrap();
        queue
            .add_printer(printer("cutter", &["print", "cut"]))
            .unwrap();
        let job = queue.enqueue(
            JobSpec::named("restricted")
                .requiring(["cut"])
                .restricted_to(["cutter"]),
        );

        assert_eq!(
            queue.route_next(),
            Some(Route {
                job_id: job,
                printer_id: "cutter".into()
            })
        );
        assert!(matches!(
            queue.printer("cutter").unwrap().status,
            PrinterStatus::Busy { job_id } if job_id == job
        ));
    }

    #[test]
    fn progress_completion_and_failure_release_printers() {
        let mut queue = JobQueue::new();
        queue.add_printer(printer("a", &[])).unwrap();
        let completed = queue.enqueue(JobSpec::named("complete"));
        queue.route_next();
        queue.update_progress(completed, 42).unwrap();
        assert_eq!(queue.job(completed).unwrap().progress_percent, 42);
        queue.complete(completed).unwrap();
        assert_eq!(queue.job(completed).unwrap().status, JobStatus::Done);
        assert_eq!(queue.job(completed).unwrap().progress_percent, 100);
        assert_eq!(queue.printer("a").unwrap().status, PrinterStatus::Idle);

        let failed = queue.enqueue(JobSpec::named("fail"));
        queue.route_next();
        queue.fail(failed, "paper jam").unwrap();
        assert_eq!(queue.job(failed).unwrap().status, JobStatus::Error);
        assert_eq!(
            queue.job(failed).unwrap().error.as_deref(),
            Some("paper jam")
        );
        assert_eq!(queue.printer("a").unwrap().status, PrinterStatus::Idle);
    }

    #[test]
    fn cancelling_queued_and_running_jobs_maintains_invariants() {
        let mut queue = JobQueue::new();
        queue.add_printer(printer("a", &[])).unwrap();
        let running = queue.enqueue(JobSpec::named("running"));
        let queued = queue.enqueue(JobSpec::named("queued"));
        queue.route_next();

        queue.cancel(queued).unwrap();
        assert!(queue.queued_job_ids().next().is_none());
        assert_eq!(queue.job(queued).unwrap().status, JobStatus::Cancelled);

        queue.cancel(running).unwrap();
        assert_eq!(queue.job(running).unwrap().status, JobStatus::Cancelled);
        assert_eq!(queue.printer("a").unwrap().status, PrinterStatus::Idle);
        assert!(queue.cancel(running).is_err());
    }

    #[test]
    fn retry_resets_transient_state_and_increments_attempts_on_route() {
        let mut queue = JobQueue::new();
        queue.add_printer(printer("a", &[])).unwrap();
        let job = queue.enqueue(JobSpec::named("retry"));
        queue.route_next();
        queue.update_progress(job, 70).unwrap();
        queue.fail(job, "disconnect").unwrap();

        queue.retry(job).unwrap();
        let retried = queue.job(job).unwrap();
        assert_eq!(retried.status, JobStatus::Queued);
        assert_eq!(retried.progress_percent, 0);
        assert_eq!(retried.assigned_printer, None);
        assert_eq!(retried.error, None);
        queue.route_next();
        assert_eq!(queue.job(job).unwrap().attempts, 2);
    }

    #[test]
    fn offline_busy_printer_errors_job_then_can_return_online() {
        let mut queue = JobQueue::new();
        queue.add_printer(printer("a", &[])).unwrap();
        let job = queue.enqueue(JobSpec::named("offline"));
        queue.route_next();

        assert_eq!(
            queue.set_printer_offline("a", "printer disconnected"),
            Ok(Some(job))
        );
        assert_eq!(queue.job(job).unwrap().status, JobStatus::Error);
        assert_eq!(queue.printer("a").unwrap().status, PrinterStatus::Offline);
        queue.retry(job).unwrap();
        assert!(queue.route_next().is_none());
        queue.set_printer_online("a").unwrap();
        assert_eq!(queue.route_next().unwrap().job_id, job);
    }

    #[test]
    fn rejects_invalid_transitions_progress_and_duplicate_printers() {
        let mut queue = JobQueue::new();
        queue.add_printer(printer("a", &[])).unwrap();
        assert!(matches!(
            queue.add_printer(printer("a", &[])),
            Err(QueueError::DuplicatePrinter(_))
        ));
        let job = queue.enqueue(JobSpec::named("invalid"));
        assert!(matches!(
            queue.update_progress(job, 101),
            Err(QueueError::InvalidProgress(101))
        ));
        assert!(matches!(
            queue.complete(job),
            Err(QueueError::InvalidJobTransition { .. })
        ));
        assert!(matches!(
            queue.cancel(999),
            Err(QueueError::UnknownJob(999))
        ));
        let mut busy = printer("busy", &[]);
        busy.status = PrinterStatus::Busy { job_id: 999 };
        assert_eq!(
            queue.add_printer(busy),
            Err(QueueError::BusyPrinterRegistration)
        );
    }

    #[test]
    fn serde_round_trip_preserves_routing_state_and_next_id() {
        let mut queue = JobQueue::new();
        queue.add_printer(printer("a", &["cut"])).unwrap();
        let first = queue.enqueue(JobSpec::named("first").requiring(["cut"]));
        queue.route_next();
        queue.update_progress(first, 25).unwrap();

        let json = serde_json::to_string(&queue).unwrap();
        let mut restored: JobQueue = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, queue);
        let second = restored.enqueue(JobSpec::named("second"));
        assert_eq!(second, first + 1);
    }

    #[test]
    fn deserialize_rejects_correlated_state_corruption() {
        let mut queue = JobQueue::new();
        queue.add_printer(printer("a", &[])).unwrap();
        let job = queue.enqueue(JobSpec::named("queued"));

        let mut value = serde_json::to_value(&queue).unwrap();
        value["queued"] = serde_json::json!([job, job]);
        assert!(serde_json::from_value::<JobQueue>(value).is_err());

        let mut value = serde_json::to_value(&queue).unwrap();
        value["next_job_id"] = serde_json::json!(job);
        assert!(serde_json::from_value::<JobQueue>(value).is_err());

        let mut value = serde_json::to_value(&queue).unwrap();
        value["jobs"][job.to_string()]["progress_percent"] = serde_json::json!(255);
        assert!(serde_json::from_value::<JobQueue>(value).is_err());

        let mut value = serde_json::to_value(&queue).unwrap();
        value["printers"][0]["status"] = serde_json::json!({ "state": "busy", "job_id": 999 });
        assert!(serde_json::from_value::<JobQueue>(value).is_err());
    }
}
