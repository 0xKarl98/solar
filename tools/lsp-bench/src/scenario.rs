use clap::ValueEnum;
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Scenario {
    SlowTyping,
    FastTyping,
    CrossFileEdits,
    RequestsDuringEdit,
}

impl Scenario {
    pub(crate) const ALL: [Self; 4] =
        [Self::SlowTyping, Self::FastTyping, Self::CrossFileEdits, Self::RequestsDuringEdit];

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::SlowTyping => "slow-typing",
            Self::FastTyping => "fast-typing",
            Self::CrossFileEdits => "cross-file-edits",
            Self::RequestsDuringEdit => "requests-during-edit",
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub(crate) enum Selection {
    All,
    SlowTyping,
    FastTyping,
    CrossFileEdits,
    RequestsDuringEdit,
}

impl Selection {
    pub(crate) fn scenarios(self) -> Vec<Scenario> {
        match self {
            Self::All => Scenario::ALL.to_vec(),
            Self::SlowTyping => vec![Scenario::SlowTyping],
            Self::FastTyping => vec![Scenario::FastTyping],
            Self::CrossFileEdits => vec![Scenario::CrossFileEdits],
            Self::RequestsDuringEdit => vec![Scenario::RequestsDuringEdit],
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RunSpec {
    pub(crate) label: &'static str,
    pub(crate) binary: PathBuf,
    pub(crate) scenario: Scenario,
    pub(crate) repetition: usize,
}

pub(crate) fn comparison_plan(
    baseline: &Path,
    candidate: &Path,
    scenarios: &[Scenario],
    repeat: usize,
) -> Vec<RunSpec> {
    let mut runs = Vec::with_capacity(scenarios.len() * repeat * 2);
    for &scenario in scenarios {
        for repetition in 0..repeat {
            let pair = if repetition % 2 == 0 {
                [("baseline", baseline), ("candidate", candidate)]
            } else {
                [("candidate", candidate), ("baseline", baseline)]
            };
            runs.extend(pair.map(|(label, binary)| RunSpec {
                label,
                binary: binary.to_path_buf(),
                scenario,
                repetition,
            }));
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_selects_every_core_scenario() {
        assert_eq!(
            Selection::All.scenarios(),
            [
                Scenario::SlowTyping,
                Scenario::FastTyping,
                Scenario::CrossFileEdits,
                Scenario::RequestsDuringEdit,
            ]
        );
    }

    #[test]
    fn comparison_plan_alternates_binary_order() {
        let baseline = PathBuf::from("baseline");
        let candidate = PathBuf::from("candidate");
        let plan = comparison_plan(&baseline, &candidate, &[Scenario::SlowTyping], 3);
        let labels = plan.iter().map(|run| run.label).collect::<Vec<_>>();

        assert_eq!(
            labels,
            ["baseline", "candidate", "candidate", "baseline", "baseline", "candidate"]
        );
    }
}
