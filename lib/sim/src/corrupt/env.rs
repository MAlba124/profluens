//! The environment variables every gate reads, parsed once into plain data.

use std::fmt;

use crate::prng::Prng;

/// A variable set to something unusable. A typo would otherwise turn a replay into a full run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct EnvError {
    pub name: &'static str,
    pub value: String,
}

impl fmt::Display for EnvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} is `{}`, expected a decimal or 0x hex number", self.name, self.value)
    }
}

impl std::error::Error for EnvError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Env {
    pub iters: Option<u64>,
    pub seed: Option<u64>,
    pub case: Option<u64>,
    pub timeout_secs: u64,
    pub dump: Option<String>,
    pub show_tolerated: bool,
}

pub(super) const DEFAULT_TIMEOUT_SECS: u64 = 120;

impl Default for Env {
    fn default() -> Self {
        Env {
            iters: None,
            seed: None,
            case: None,
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            dump: None,
            show_tolerated: false,
        }
    }
}

type Lookup<'a> = &'a dyn Fn(&str) -> Option<String>;

fn number(name: &'static str, get: Lookup) -> Result<Option<u64>, EnvError> {
    let Some(value) = get(name) else {
        return Ok(None);
    };
    let text = value.trim();
    let parsed = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => text.parse(),
    };
    parsed.map(Some).map_err(|_| EnvError { name, value })
}

impl Env {
    pub fn parse(get: Lookup) -> Result<Env, EnvError> {
        let seed = match number("PF_FUZZ_SEED", get)? {
            Some(seed) => Some(seed),
            None => number("PF_FUZZ_BASE", get)?,
        };
        // A zero is off, not a soak of no cases or a watchdog that fires at its first tick.
        let on = |n: &u64| *n > 0;
        Ok(Env {
            iters: number("PF_FUZZ_ITERS", get)?.filter(on),
            seed,
            case: number("PF_FUZZ_CASE", get)?,
            timeout_secs: number("PF_FUZZ_TIMEOUT_SECS", get)?
                .filter(on)
                .unwrap_or(DEFAULT_TIMEOUT_SECS),
            dump: get("PF_FUZZ_DUMP"),
            show_tolerated: get("PF_FUZZ_SHOW_TOLERATED").is_some_and(|v| v.trim() != "0"),
        })
    }

    /// The process environment. Panics on a variable that does not parse, and says which
    /// `PF_FUZZ_` names no gate reads, so a misspelt knob is not silently ignored.
    pub fn read() -> Env {
        for (name, _) in std::env::vars_os() {
            let name = name.to_string_lossy();
            if name.starts_with("PF_FUZZ_") && !KNOWN.contains(&name.as_ref()) {
                eprintln!("{name} is set and no corruption gate reads it, the knobs are {KNOWN:?}");
            }
        }
        let get = |name: &str| std::env::var_os(name).map(|v| v.to_string_lossy().into_owned());
        match Env::parse(&get) {
            Ok(env) => env,
            Err(error) => panic!("{error}"),
        }
    }

    pub fn soak(&self) -> bool {
        self.iters.is_some()
    }
}

const KNOWN: [&str; 7] = [
    "PF_FUZZ_SEED",
    "PF_FUZZ_BASE",
    "PF_FUZZ_ITERS",
    "PF_FUZZ_CASE",
    "PF_FUZZ_TIMEOUT_SECS",
    "PF_FUZZ_DUMP",
    "PF_FUZZ_SHOW_TOLERATED",
];

// One seed and derivation per kind of gate, so gates draw the cases they drew before the merge.
const SITE_SEED: u64 = 0x005E_ED0F_C0FF_EE00;
const CORPUS_SEED: u64 = 0x5EED_C0DE_C0FF_EE00;

pub(super) fn site_case_seed(env: &Env, name: &str, index: u64) -> u64 {
    let root = Prng::new(env.seed.unwrap_or(SITE_SEED)).substream_named(name);
    root.substream(index).next_u64()
}

pub(super) fn corpus_base_seed(env: &Env, name: &str) -> u64 {
    Prng::new(env.seed.unwrap_or(CORPUS_SEED)).substream_named(name).next_u64()
}

pub(super) fn corpus_case_seed(env: &Env, name: &str, index: u64) -> u64 {
    Prng::new(corpus_base_seed(env, name)).substream(index).next_u64()
}

pub fn iterations(default: u64) -> u64 {
    Env::read().iters.unwrap_or(default)
}

pub fn base_seed(name: &str) -> u64 {
    corpus_base_seed(&Env::read(), name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(vars: &[(&str, &str)]) -> Result<Env, EnvError> {
        Env::parse(&|name| vars.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string()))
    }

    #[test]
    fn nothing_set_is_the_defaults() {
        assert_eq!(env(&[]), Ok(Env::default()));
        assert!(!Env::default().soak());
    }

    #[test]
    fn every_variable_lands_in_its_field() {
        let got = env(&[
            ("PF_FUZZ_ITERS", "40000"),
            ("PF_FUZZ_SEED", " 0x5EED "),
            ("PF_FUZZ_CASE", "0XdeadBEEF"),
            ("PF_FUZZ_TIMEOUT_SECS", "7"),
            ("PF_FUZZ_DUMP", "/tmp/dump dir"),
            ("PF_FUZZ_SHOW_TOLERATED", ""),
        ]);
        let want = Env {
            iters: Some(40_000),
            seed: Some(0x5EED),
            case: Some(0xDEAD_BEEF),
            timeout_secs: 7,
            dump: Some("/tmp/dump dir".to_string()),
            show_tolerated: true,
        };
        assert_eq!(got, Ok(want));
        assert!(got.is_ok_and(|env| env.soak()));
    }

    #[test]
    fn a_zero_is_off() {
        let got = env(&[
            ("PF_FUZZ_ITERS", "0"),
            ("PF_FUZZ_TIMEOUT_SECS", "0"),
            ("PF_FUZZ_SHOW_TOLERATED", "0"),
        ]);
        assert_eq!(got, Ok(Env::default()));
    }

    #[test]
    fn the_older_base_variable_still_reseeds_and_the_newer_one_wins() {
        assert_eq!(env(&[("PF_FUZZ_BASE", "9")]).map(|e| e.seed), Ok(Some(9)));
        let both = env(&[("PF_FUZZ_BASE", "9"), ("PF_FUZZ_SEED", "3")]);
        assert_eq!(both.map(|e| e.seed), Ok(Some(3)));
    }

    #[test]
    fn a_value_that_is_not_a_number_is_an_error_naming_the_variable() {
        let bad = [("PF_FUZZ_CASE", "0xZZ"), ("PF_FUZZ_ITERS", "-1"), ("PF_FUZZ_SEED", "")];
        for (name, value) in bad {
            let error = env(&[(name, value)]).expect_err("must not parse");
            assert_eq!((error.name, error.value.as_str()), (name, value));
            assert!(error.to_string().starts_with(name));
        }
    }

    #[test]
    fn case_seeds_follow_the_base_seed_the_gate_name_and_the_index() {
        let quiet = Env::default();
        let moved = Env { seed: Some(1), ..Env::default() };
        for seed_of in [site_case_seed, corpus_case_seed] {
            assert_eq!(seed_of(&quiet, "a", 0), seed_of(&quiet, "a", 0));
            assert_ne!(seed_of(&quiet, "a", 0), seed_of(&quiet, "a", 1));
            assert_ne!(seed_of(&quiet, "a", 0), seed_of(&quiet, "b", 0));
            assert_ne!(seed_of(&quiet, "a", 0), seed_of(&moved, "a", 0));
        }
    }

    #[test]
    fn default_case_seeds_are_the_ones_the_per_crate_rigs_drew() {
        // Pinned. A change here moves every gate onto cases it has never run.
        assert_eq!(site_case_seed(&Env::default(), "mkv reader", 0), SITE_PIN);
        assert_eq!(corpus_case_seed(&Env::default(), "flac_stream_decoder", 0), CORPUS_PIN);
    }

    const SITE_PIN: u64 = 3_147_398_400_819_555_054;
    const CORPUS_PIN: u64 = 11_180_766_621_887_575_611;
}
