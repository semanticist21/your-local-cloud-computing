use super::*;

#[derive(Clone, Copy)]
pub(super) struct CpuObservation {
    pub at: u64,
    pub percent: Option<f64>,
}

pub(super) struct CpuGate {
    blocked: bool,
    observed: Option<u64>,
    high_since: Option<u64>,
    low_since: Option<u64>,
}
impl Default for CpuGate {
    fn default() -> Self {
        Self {
            blocked: true,
            observed: None,
            high_since: None,
            low_since: None,
        }
    }
}
impl CpuGate {
    fn evaluate(
        &mut self,
        guard: &CpuGuard,
        sample: Option<CpuObservation>,
        at: u64,
    ) -> Option<String> {
        let Some(sample) = sample.filter(|s| at >= s.at && at - s.at <= 60 && s.percent.is_some())
        else {
            *self = Self::default();
            return Some("CPU measurement unavailable".into());
        };
        if self.observed != Some(sample.at) {
            if self
                .observed
                .is_some_and(|previous| sample.at < previous || sample.at - previous > 30)
            {
                self.high_since = None;
                self.low_since = None;
            }
            self.observed = Some(sample.at);
            let cpu = sample.percent.unwrap();
            if cpu >= guard.pause_above {
                let since = *self.high_since.get_or_insert(sample.at);
                self.low_since = None;
                if sample.at - since >= 30 {
                    self.blocked = true;
                }
            } else if cpu <= guard.resume_below {
                let since = *self.low_since.get_or_insert(sample.at);
                self.high_since = None;
                if sample.at - since >= 30 {
                    self.blocked = false;
                }
            } else {
                self.high_since = None;
                self.low_since = None;
            }
        }
        self.blocked.then(|| "Waiting for CPU recovery".into())
    }
}

fn preparing(status: &EnvironmentStatus) -> bool {
    matches!(status.state.as_str(), "preparing" | "unknown")
}
fn occupied(worker: &str, jobs: &HashMap<String, usize>, statuses: &[EnvironmentStatus]) -> usize {
    jobs.get(worker).copied().unwrap_or_default()
        + usize::from(
            statuses
                .iter()
                .any(|status| status.worker_id == worker && preparing(status)),
        )
}

impl Store {
    pub(super) fn host_job_counts(&self) -> Result<HashMap<String, usize>> {
        let mut statement = self.db.prepare(
            "SELECT json_extract(data,'$.worker_id'), COUNT(*) FROM jobs
             WHERE json_extract(data,'$.state') IN ('assigned','waiting','running','cancelling','unknown')
             AND json_extract(data,'$.worker_id') IS NOT NULL GROUP BY json_extract(data,'$.worker_id')"
        )?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
    pub(super) fn host_configs(&self) -> Result<Vec<(String, HostConfig)>> {
        let mut statement = self.db.prepare("SELECT id,data FROM hosts ORDER BY id")?;
        let rows =
            statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.map(|row| {
            let (id, data) = row?;
            Ok((id, serde_json::from_str(&data)?))
        })
        .collect()
    }

    pub(super) fn host_infos(&mut self, at: u64) -> Result<Vec<HostInfo>> {
        let workers = self.workers()?;
        let jobs = self.host_job_counts()?;
        let statuses = self.statuses()?;
        let mut configs = self.host_configs()?;
        let grouped: BTreeSet<_> = configs
            .iter()
            .flat_map(|(_, c)| c.workers.iter().cloned())
            .collect();
        let mut result = Vec::new();
        for worker in &workers {
            if !grouped.contains(&worker.id) {
                configs.push((
                    format!("worker:{}", worker.id),
                    HostConfig {
                        workers: vec![worker.id.clone()],
                        max_jobs: worker.max_jobs,
                        cpu_guard: None,
                    },
                ));
            }
        }
        for (id, config) in configs {
            let members: Vec<_> = workers
                .iter()
                .filter(|w| config.workers.contains(&w.id))
                .collect();
            let used = config
                .workers
                .iter()
                .map(|w| occupied(w, &jobs, &statuses))
                .sum::<usize>();
            let eligible: Vec<_> = members
                .iter()
                .filter(|w| {
                    w.online
                        && !w.paused
                        && w.updating.is_none()
                        && self.sessions.get(&w.id).is_some_and(|s| !s.reconciling)
                })
                .collect();
            let capacity = config
                .max_jobs
                .min(eligible.iter().map(|w| w.max_jobs).sum());
            let worker_slots: usize = eligible
                .iter()
                .map(|w| {
                    if statuses.iter().any(|s| s.worker_id == w.id && preparing(s)) {
                        0
                    } else {
                        w.max_jobs.saturating_sub(occupied(&w.id, &jobs, &statuses))
                    }
                })
                .sum();
            let source = config
                .cpu_guard
                .as_ref()
                .map(|g| g.metrics_worker.as_str())
                .or_else(|| (members.len() == 1).then(|| members[0].id.as_str()));
            let sample = source.and_then(|source| {
                members.iter().find(|w| {
                    w.id == source
                        && w.online
                        && (config.cpu_guard.is_none()
                            || (!w.capabilities.containerized && w.capabilities.cpu_metrics))
                        && self.sessions.get(source).is_some_and(|s| !s.reconciling)
                })?;
                self.cpu_samples.get(source).copied()
            });
            let fresh = sample.filter(|s| at >= s.at && at - s.at <= 60 && s.percent.is_some());
            let guard_reason = config.cpu_guard.as_ref().and_then(|guard| {
                self.cpu_gates
                    .entry(id.clone())
                    .or_default()
                    .evaluate(guard, sample, at)
            });
            let free = config.max_jobs.saturating_sub(used).min(worker_slots);
            let reason = guard_reason.or_else(|| {
                (free == 0).then(|| "Host concurrency limit or no available worker slots".into())
            });
            result.push(HostInfo {
                managed: !id.starts_with("worker:"),
                id,
                config,
                occupied: used,
                capacity,
                available_slots: if reason.is_none() { free } else { 0 },
                cpu_percent: fresh.and_then(|s| s.percent),
                cpu_at: sample.map(|s| s.at),
                schedulable: reason.is_none(),
                wait_reason: reason,
            });
        }
        Ok(result)
    }

    pub(super) fn set_host_config(
        &mut self,
        id: &str,
        mut config: HostConfig,
    ) -> ApiResult<HostInfo> {
        identifier(id).map_err(invalid)?;
        if config.workers.is_empty()
            || config.workers.len() > 256
            || !(1..=256).contains(&config.max_jobs)
        {
            return Err(invalid(
                "host requires 1..256 workers and max_jobs between 1 and 256",
            ));
        }
        let original_len = config.workers.len();
        config.workers.sort();
        config.workers.dedup();
        if config.workers.len() != original_len {
            return Err(invalid("duplicate worker membership"));
        }
        let workers = self.workers()?;
        for worker in &config.workers {
            if !workers.iter().any(|w| &w.id == worker) {
                return Err(invalid(format!("unknown worker {worker}")));
            }
        }
        let configs = self.host_configs()?;
        for (other_id, other) in &configs {
            if other_id != id && config.workers.iter().any(|w| other.workers.contains(w)) {
                return Err(invalid(format!(
                    "worker already belongs to host {other_id}; remove it there first"
                )));
            }
        }
        if let Some(guard) = &config.cpu_guard {
            if !guard.pause_above.is_finite()
                || !guard.resume_below.is_finite()
                || !(0.0..=100.0).contains(&guard.resume_below)
                || !(0.0..=100.0).contains(&guard.pause_above)
                || guard.resume_below >= guard.pause_above
            {
                return Err(invalid(
                    "CPU thresholds require 0 <= resume_below < pause_above <= 100",
                ));
            }
            if !config.workers.contains(&guard.metrics_worker) {
                return Err(invalid("metrics_worker must belong to this host"));
            }
            let source = workers
                .iter()
                .find(|w| w.id == guard.metrics_worker)
                .unwrap();
            if source.capabilities.containerized || !source.capabilities.cpu_metrics {
                return Err(invalid(
                    "metrics_worker must be a native worker reporting CPU validity; upgrade older workers first",
                ));
            }
        }
        let previous = configs
            .iter()
            .find(|(other, _)| other == id)
            .map(|(_, c)| c);
        let before: BTreeSet<_> = previous
            .into_iter()
            .flat_map(|c| c.workers.iter())
            .collect();
        let after: BTreeSet<_> = config.workers.iter().collect();
        let jobs = self.host_job_counts()?;
        let statuses = self.statuses()?;
        for worker in before.symmetric_difference(&after) {
            if occupied(worker, &jobs, &statuses) != 0 {
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    format!(
                        "worker {worker} has active or unresolved work; drain it before changing hosts"
                    ),
                ));
            }
        }
        if previous.and_then(|c| c.cpu_guard.as_ref()) != config.cpu_guard.as_ref() {
            self.cpu_gates.remove(id);
        }
        self.db.execute("INSERT INTO hosts(id,data) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET data=excluded.data",
            params![id, encode(&config)?]).map_err(anyhow::Error::from)?;
        self.schedule()?;
        self.host_infos(now())?
            .into_iter()
            .find(|h| h.id == id)
            .ok_or_else(|| missing("host"))
    }

    pub(super) fn delete_host_config(&mut self, id: &str) -> ApiResult<()> {
        let Some((_, config)) = self
            .host_configs()?
            .into_iter()
            .find(|(other, _)| other == id)
        else {
            return Ok(());
        };
        let jobs = self.host_job_counts()?;
        let statuses = self.statuses()?;
        if config
            .workers
            .iter()
            .any(|w| occupied(w, &jobs, &statuses) > 0)
        {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "host has active or unresolved work; drain it before deleting".into(),
            ));
        }
        self.db
            .execute("DELETE FROM hosts WHERE id=?1", [id])
            .map_err(anyhow::Error::from)?;
        self.cpu_gates.remove(id);
        self.schedule()?;
        Ok(())
    }
}

pub(super) async fn hosts(State(state): State<Shared>) -> ApiResult<Json<Vec<HostInfo>>> {
    Ok(Json(lock(&state)?.host_infos(now())?))
}
pub(super) async fn set_host(
    State(state): State<Shared>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    Json(config): Json<HostConfig>,
) -> ApiResult<Json<HostInfo>> {
    let mut store = lock(&state)?;
    let info = store.set_host_config(&id, config)?;
    store.record(&principal, "host.set", &id, Some(encode(&info.config)?));
    Ok(Json(info))
}
pub(super) async fn delete_host(
    State(state): State<Shared>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let mut store = lock(&state)?;
    store.delete_host_config(&id)?;
    store.record(&principal, "host.delete", &id, None);
    Ok(Json(serde_json::json!({"deleted":id})))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn guard() -> CpuGuard {
        CpuGuard {
            metrics_worker: "one".into(),
            pause_above: 90.0,
            resume_below: 75.0,
        }
    }
    fn sample(at: u64, percent: f64) -> Option<CpuObservation> {
        Some(CpuObservation {
            at,
            percent: Some(percent),
        })
    }
    #[test]
    fn cpu_gate_requires_continuous_samples_and_hysteresis() {
        let mut gate = CpuGate::default();
        for at in [0, 15] {
            assert!(gate.evaluate(&guard(), sample(at, 75.0), at).is_some());
        }
        assert!(gate.evaluate(&guard(), sample(30, 75.0), 30).is_none());
        for at in [45, 60] {
            assert!(gate.evaluate(&guard(), sample(at, 90.0), at).is_none());
        }
        assert!(gate.evaluate(&guard(), sample(75, 90.0), 75).is_some());
        for at in [90, 105] {
            assert!(gate.evaluate(&guard(), sample(at, 70.0), at).is_some());
        }
        assert!(gate.evaluate(&guard(), sample(120, 80.0), 120).is_some());
        for at in [135, 150] {
            assert!(gate.evaluate(&guard(), sample(at, 70.0), at).is_some());
        }
        assert!(gate.evaluate(&guard(), sample(165, 70.0), 165).is_none());
        assert!(gate.evaluate(&guard(), sample(165, 70.0), 226).is_some());
        assert!(gate.evaluate(&guard(), sample(240, 70.0), 240).is_some());
        assert!(gate.evaluate(&guard(), sample(271, 70.0), 271).is_some());
        assert!(gate.evaluate(&guard(), sample(286, 70.0), 286).is_some());
        assert!(gate.evaluate(&guard(), sample(301, 70.0), 301).is_none());
        assert!(
            gate.evaluate(
                &guard(),
                Some(CpuObservation {
                    at: 316,
                    percent: None
                }),
                316
            )
            .is_some()
        );
        assert!(gate.evaluate(&guard(), None, 330).is_some());
    }
    #[test]
    fn repeated_snapshot_cannot_complete_a_cpu_window() {
        let mut gate = CpuGate::default();
        assert!(gate.evaluate(&guard(), sample(10, 20.0), 10).is_some());
        assert!(gate.evaluate(&guard(), sample(10, 20.0), 40).is_some());
        assert!(gate.evaluate(&guard(), sample(40, 20.0), 40).is_none());
        assert!(
            CpuGate::default()
                .evaluate(&guard(), sample(40, 20.0), 40)
                .is_some()
        );
    }
}
