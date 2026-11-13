use rosen_core::ObjectId;
use rosen_protocol::{BisectCulprit, BisectJob, BisectState, ErrorBody, ErrorCode};

pub(super) struct Session {
    pub id: i64,
    pub commits: Vec<(ObjectId, Option<String>)>,
    pub low: usize,
    pub high: usize,
    pub phase: String,
    pub step: u64,
    pub output: String,
}

/// Balanced split of the remaining possible first failures.
pub fn midpoint(low: usize, high: usize) -> usize {
    low + (high - low) / 2
}

impl Session {
    pub fn job_index(&self) -> Option<usize> {
        match self.phase.as_str() {
            "search" => Some(midpoint(self.low, self.high)),
            "confirm" => Some(self.high),
            "parent" => Some(self.high - 1),
            _ => None,
        }
    }

    pub fn state(&self) -> Result<BisectState, ErrorBody> {
        let culprit = if self.phase == "done" {
            Some(BisectCulprit {
                commit: self.commits[self.high].0,
                parent: self.commits[self.high - 1].0,
                change: self.commits[self.high].1.clone().ok_or_else(|| {
                    ErrorBody::new(ErrorCode::Internal, "culprit has no landed change")
                })?,
                output: self.output.clone(),
            })
        } else {
            None
        };
        Ok(BisectState {
            id: self.id,
            job: self.job_index().map(|index| BisectJob {
                session: self.id,
                step: self.step,
                commit: self.commits[index].0,
            }),
            culprit,
            flaky: self.phase == "flaky",
        })
    }

    pub fn advance(&mut self, passed: bool, output: &str) {
        match self.phase.as_str() {
            "search" => {
                let middle = midpoint(self.low, self.high);
                if passed {
                    self.low = middle;
                } else {
                    self.high = middle;
                    self.output = output.to_owned();
                }
                if self.high - self.low == 1 {
                    self.phase = "confirm".into();
                }
            }
            "confirm" if !passed => {
                self.output = output.to_owned();
                self.phase = "parent".into();
            }
            "parent" if passed => self.phase = "done".into(),
            _ => self.phase = "flaky".into(),
        }
        self.step += 1;
    }
}
