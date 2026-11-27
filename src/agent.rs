//! The LLM integration layer (Phase 4 of the design).
//!
//! The fixpoint never contains an LLM call: extraction happens at the
//! *ingestion boundary* (the [`Extractor`] trait — a real deployment plugs an
//! OpenIE LLM here, tests and examples use [`MockExtractor`]), update
//! decisions are deterministic rules first (Mem0-style
//! ADD/UPDATE/NOOP/escalate), and derivation runs asynchronously via
//! `maintain()`. The [`ContextAssembler`] places distilled facts at the top
//! of the window and verbatim provenance at the bottom (lost-in-the-middle
//! mitigation) under a token budget.

use crate::eval::{Ann, Engine};
use crate::intern::Term;
use crate::intern::Value;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::fmt::Write as _;

/// A conversational episode: the unit of ingestion and provenance.
#[derive(Debug, Clone)]
pub struct Episode {
    pub id: String,
    pub text: String,
    pub ts: i64,
    /// The identified speaker, when the application knows it: first-person
    /// references in the episode resolve to this entity during extraction.
    pub speaker: Option<String>,
}

/// One candidate fact produced by extraction.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateFact {
    pub subj: String,
    pub pred: String,
    pub obj: String,
    pub confidence: f64,
}

/// The extraction boundary. Implementations call an LLM in production;
/// they must be memoizable by (episode, extractor-version).
pub trait Extractor {
    fn extract(&mut self, episode: &Episode) -> Vec<CandidateFact>;

    /// Observability: (model calls, failures). Defaults to zero for
    /// deterministic extractors.
    fn stats(&self) -> (usize, usize) {
        (0, 0)
    }
}

/// Boxed extractors are extractors: lets callers swap implementations
/// (live vs file-cached) behind one `AgentMemory` type.
impl Extractor for Box<dyn Extractor> {
    fn extract(&mut self, episode: &Episode) -> Vec<CandidateFact> {
        (**self).extract(episode)
    }

    fn stats(&self) -> (usize, usize) {
        (**self).stats()
    }
}

/// Deterministic stand-in for the LLM OpenIE step: parses `S --rel--> O`
/// lines at fixed confidence. Used by tests and examples.
pub struct MockExtractor {
    pub confidence: f64,
    seen: HashMap<String, Vec<CandidateFact>>,
}

impl MockExtractor {
    pub fn new(confidence: f64) -> Self {
        MockExtractor {
            confidence,
            seen: HashMap::new(),
        }
    }
}

impl Extractor for MockExtractor {
    fn extract(&mut self, episode: &Episode) -> Vec<CandidateFact> {
        // memoized by episode id: never re-extracted
        if let Some(cached) = self.seen.get(&episode.id) {
            return cached.clone();
        }
        let out = parse_protocol(episode.text.as_str(), self.confidence);
        self.seen.insert(episode.id.clone(), out.clone());
        out
    }
}

/// Why an entity token fails strict validation, as a self-correcting
/// reason (echoed to the model that produced it).
/// A character that can be seen in a name: everything except control characters, whitespace
/// and the Unicode invisible/format set (zero-width, bidi overrides, BOM).
fn is_visible(c: char) -> bool {
    if c.is_control() || c.is_whitespace() {
        return false;
    }
    !matches!(
        c as u32,
        0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x2069 | 0xFEFF
    )
}

/// Characters that end a sentence or bracket a list. Prose in this store is signalled by these,
/// so they stay refused inside a spaced name; everything else visible (`± ° → ≤ = %`) is a unit
/// or an operator and is legitimate (`transform 3 = 270°`, `port 3012→3002`, `≤200 ms`).
fn allowed_in_spaced_name(c: char) -> bool {
    // the space is the point of a spaced name, and is_visible() excludes whitespace
    // `?` and `!` belong here: an existing engine test caught `products? maybe not because
    // generic` sailing through the first version of this predicate.
    c == ' '
        || (is_visible(c)
            && !matches!(c, ',' | ';' | '?' | '!' | '(' | ')' | '[' | ']' | '{' | '}' | '"' | '`'))
}

/// True when a name looks like a credential rather than a value.
///
/// Deliberately mirrors `is_secret_shaped` in `~/.hermes/scripts/lemmalog-facts-lib.py` — the
/// engine carries no regex dependency and the Python copy owns the export boundary. The two are
/// pinned by the SAME case list on both sides (`secret_shape_parity_cases` here,
/// `test_secret_shapes_are_refused_and_values_kept` there), because a hand-mirrored rule drifts.
///
/// Pure hex is NEVER secret: `artifact --sha--> 9f2c…` is a legitimate fact shape here.
pub fn secret_shaped(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    const PREFIXES: [&str; 8] = [
        "sk_live_", "sk_test_", "rk_live_", "pk_live_", "sk-", "glpat-", "github_pat_", "hf_",
    ];
    for p in PREFIXES {
        if lower.starts_with(p) && s.len() > p.len() + 7 {
            return true;
        }
    }
    for p in ["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "xoxb-", "xoxp-", "xoxa-", "xoxo-", "xoxr-"] {
        if lower.starts_with(p) && s.len() > p.len() + 9 {
            return true;
        }
    }
    for p in ["AKIA", "ASIA"] {
        let rest = &s[p.len().min(s.len())..];
        if s.starts_with(p) && rest.len() == 16 && rest.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) {
            return true;
        }
    }
    if let Some(rest) = s.strip_prefix("AIza") {
        if rest.len() == 35 && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return true;
        }
    }
    if s.starts_with("eyJ") && s.contains('.') && s.len() > 20 {
        return true; // JWT
    }
    if s.contains("BEGIN ") && s.contains("PRIVATE KEY") {
        return true; // PEM
    }
    if let Some(rest) = lower.strip_prefix("bearer ") {
        if rest.len() >= 12 && !rest.contains(' ') {
            return true;
        }
    }
    // `scheme://user:password@host` — a URL is a legal fact object, the password rides along.
    // Measured 2026-11-22: this shape passed every other pattern in the export filter.
    if let Some((_, after)) = s.split_once("://") {
        let authority = after.split('/').next().unwrap_or("");
        if let Some((creds, _host)) = authority.split_once('@') {
            if let Some((user, pw)) = creds.split_once(':') {
                if !user.is_empty() && !pw.is_empty() {
                    return true;
                }
            }
        }
    }
    // `token=`/`auth=`/`bearer=` are as often config words as credentials (`auth=required`), so
    // they only count when the value itself is token-like — the Python side draws the same line.
    for key in ["token", "auth", "bearer"] {
        for sep in ['=', ':'] {
            let needle = format!("{key}{sep}");
            if let Some(idx) = lower.find(&needle) {
                let token = s[idx + needle.len()..]
                    .trim_start()
                    .split_whitespace()
                    .next()
                    .unwrap_or("");
                let tokenish = token.len() >= 10
                    && token.chars().any(|c| c.is_ascii_alphabetic())
                    && token.chars().any(|c| c.is_ascii_digit());
                if tokenish {
                    return true;
                }
            }
        }
    }
    for key in ["password", "passwd", "secret", "api_key", "apikey", "access_key", "private_key", "client_secret"] {
        for sep in ['=', ':'] {
            let needle = format!("{key}{sep}");
            if let Some(idx) = lower.find(&needle) {
                let value = s[idx + needle.len()..].trim_start();
                let token = value.split_whitespace().next().unwrap_or("");
                if token.len() >= 6 && token.chars().any(|c| c.is_ascii_digit()) {
                    return true;
                }
            }
        }
    }
    // The general shape: long, from the token alphabets, mixing cases and digits, not pure hex.
    if s.len() >= 32
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-' | '.'))
    {
        let has_upper = s.chars().any(|c| c.is_ascii_uppercase());
        let has_lower = s.chars().any(|c| c.is_ascii_lowercase());
        let has_digit = s.chars().any(|c| c.is_ascii_digit());
        let all_hex = s.chars().all(|c| c.is_ascii_hexdigit());
        if has_upper && has_lower && has_digit && !all_hex {
            return true;
        }
    }
    false
}

/// Relations whose object IS a hex digest of a known width — the only case where pure hex is a
/// fact rather than a credential (br-gx5.93). Mirrors `_HEX_LEN_BY_RELATION` in
/// lemmalog-facts-lib.py; tests/secret_corpus.tsv is read by both suites so the two cannot drift.
pub const HEX_DIGEST_RELATIONS: [(&str, usize); 4] =
    [("sha", 40), ("sha256", 64), ("md5", 32), ("container_id", 64)];

/// The object half of the secret rule: WHAT the object is, given the relation carrying it.
///
/// A pure-hex object of 32+ chars is a credential unless the relation names that exact digest
/// and the object is that many LOWERCASE hex chars. `secret_shaped` alone refuses every full
/// sha and misses none of the hex shapes; this is the split the decision asked for.
pub fn secret_shaped_object(obj: &str, relation: &str) -> bool {
    if secret_shaped(obj) {
        return true;
    }
    let t = obj.trim();
    if t.len() >= 32 && t.chars().all(|c| c.is_ascii_hexdigit()) {
        let lowercase = !t.chars().any(|c| c.is_ascii_uppercase());
        let rel = relation.trim().to_ascii_lowercase();
        let named = HEX_DIGEST_RELATIONS
            .iter()
            .any(|(r, n)| *r == rel.as_str() && t.len() == *n);
        return !(lowercase && named);
    }
    false
}

fn entity_token_problem(s: &str) -> Option<String> {
    // unresolved-reference words: pronouns and role placeholders that mean
    // the model failed to resolve the entity
    const BLOCKED: [&str; 14] = [
        "i", "me", "my", "mine", "speaker", "user", "they", "them", "he", "she", "it", "we", "you",
        "that",
    ];
    let lower = s.to_lowercase();
    if s.is_empty() {
        Some("empty entity name".to_string())
    } else if BLOCKED.contains(&lower.as_str()) {
        Some(format!(
            "'{s}' is a pronoun or role word — resolve it to the entity's real name"
        ))
    } else if s.len() > 60 {
        // Separate branches on purpose: a 5000-character single token used to be reported as
        // "more than 8 words", which sent the operator looking for prose that was not there.
        Some(format!(
            "too long ({} bytes, limit 60) — entity names are short",
            s.len()
        ))
    } else if s.split_whitespace().count() > 8 {
        Some(format!(
            "looks like prose ({} words, limit 8) — entity names are short",
            s.split_whitespace().count()
        ))
    } else if !s.chars().any(|c| c.is_whitespace()) && s.len() <= 60 {
        // a single token accepts all printable characters: kernel-code
        // facts need `*pmap`, `entry->next`, `MAP_FIXED|MAP_ANON`,
        // `vm_fault_entry()` — and since the line protocol already
        // parsed the line, a `-->` can't be hiding inside. Punctuation
        // plus SPACES remains the leaked-deliberation signature below.
        // Any VISIBLE character is a name character; the refusal is about invisibility, not
        // provenance. Requiring ASCII cost real facts: `270°`, `3012→3002`, `±175 chars`,
        // `≤200 ms`, `café` were all dropped by this branch (13 recorded drops), and the
        // accent was storable only when a space followed it. Control, whitespace and the
        // Unicode invisible/format characters remain refused — those are the ones that can
        // hide a second value inside one token.
        if s.chars().all(is_visible) {
            None
        } else {
            // Reached only for control, whitespace or an invisible/format character now.
            Some("contains a control or invisible character — names must be visible text".to_string())
        }
    } else if !s
        .chars()
        .all(allowed_in_spaced_name)
    {
        Some("contains sentence punctuation or invisible characters — spaced names take visible text (letters, digits, units) but not ',', ';' or brackets, which signal prose".to_string())
    } else if s.contains(' ') && s.chars().any(|c| matches!(c, '/' | '.' | ':' | '#')) {
        // Path characters are allowed so that source references
        // (`src/agent.rs:118`) can be entities, but only as single tokens:
        // punctuation *plus* spaces is the signature of leaked prose, which
        // is exactly what strict validation exists to drop.
        // Two different things land here, so the message has to cover both: a sentence with full
        // stops (`Yes. Confirmed.`) and a path/version/IP sitting mid-phrase
        // (`maps 3012→3002 to 172.18.0.6`). The old text mentioned only source references, which
        // was wrong for both.
        Some("'.', '/', ':' or '#' inside a spaced name — that is the leaked-prose or \
path-in-a-phrase signature. A path, version or IP belongs in its own single token \
(`~/.hermes/config.yaml`); a sentence is not a name"
            .to_string())
    } else {
        None
    }
}

fn valid_entity_token(s: &str) -> bool {
    entity_token_problem(s).is_none()
}

/// Strict protocol parsing for MODEL output: lines that are not exactly
/// `Entity --relation[conf]--> Entity` with clean entity tokens are
/// dropped. Reasoning models sometimes leak deliberation into the answer;
/// those lines (questions, prose, bullets) must not become facts.
pub fn parse_protocol_strict(text: &str, default_confidence: f64) -> Vec<CandidateFact> {
    parse_protocol(text, default_confidence)
        .into_iter()
        .filter(|c| {
            valid_entity_token(&c.subj)
                && valid_entity_token(&c.obj)
                && !c.pred.is_empty()
                && c.pred
                    .chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
        })
        .collect()
}

/// One line of the protocol, or a reason it cannot parse.
fn parse_line(raw: &str, default_confidence: f64) -> Result<CandidateFact, String> {
    let line = raw.trim();
    // Two facts crammed onto one line used to merge: `d --status--> four e --status--> five`
    // stored a single fact whose OBJECT was `four e --status--> five`. The line is ambiguous by
    // construction (the second `-->` could be a separator or part of the value), so refuse it
    // instead of silently absorbing one fact into another's value.
    if line.matches("-->").count() > 1 {
        return Err(
            "more than one `-->` on the line — one fact per line (`s --rel--> o`)".to_string(),
        );
    }
    let (s, rest) = line
        .split_once("--")
        .ok_or_else(|| "no `--rel-->` structure".to_string())?;
    let (rel, o) = rest
        .split_once("-->")
        .ok_or_else(|| "has `--` but no `-->`".to_string())?;
    // optional confidence suffix on the relation: `rel[0.8]`
    let (rel, conf) = match rel.trim().rsplit_once('[') {
        Some((r, c)) if c.ends_with(']') => (
            r.trim(),
            c.trim_end_matches(']')
                .trim()
                .parse::<f64>()
                .unwrap_or(default_confidence),
        ),
        _ => (rel.trim(), default_confidence),
    };
    if rel.is_empty() {
        return Err("empty relation".to_string());
    }
    // models habitually quote multi-word phrases; the protocol has no
    // quoting, so symmetric quotes become part of the symbol ("mean" ≠
    // mean). Strip them at the parse boundary.
    let unquote = |t: &str| -> String {
        let t = t.trim();
        if t.len() >= 2
            && ((t.starts_with('"') && t.ends_with('"'))
                || (t.starts_with('\'') && t.ends_with('\'')))
        {
            t[1..t.len() - 1].trim().to_string()
        } else {
            t.to_string()
        }
    };
    Ok(CandidateFact {
        subj: unquote(s),
        pred: rel.to_string(),
        obj: unquote(o),
        confidence: conf,
    })
}

/// Line protocol shared by mock and LLM extractors: each line is
/// `S --rel--> O` with optional per-fact confidence `S --rel[0.8]--> O`.
/// Unparseable lines are skipped (extraction is best-effort).
pub fn parse_protocol(text: &str, default_confidence: f64) -> Vec<CandidateFact> {
    text.lines()
        .filter_map(|l| parse_line(l, default_confidence).ok())
        .collect()
}

/// Strict parse WITH a drop report: `(facts, dropped)` where dropped is
/// `(line, reason)` for every line not asserted — parse failures and
/// strict-validation failures alike. Silent zero-fact ingestion is the
/// worst failure mode a caller can face; this makes it loud.
pub fn parse_protocol_reported(
    text: &str,
    default_confidence: f64,
) -> (Vec<CandidateFact>, Vec<(String, String)>) {
    let mut facts = Vec::new();
    let mut dropped = Vec::new();
    for raw in text.lines() {
        if raw.trim().is_empty() {
            continue;
        }
        match parse_line(raw, default_confidence) {
            Ok(c) => {
                // A credential is not a fact. Everything downstream had to remember this
                // (the export's filter, the block's renderer, the sweep's own predicate), and
                // the write path was the one place that did not (br-gx5.68).
                let secret = secret_shaped(&c.subj) || secret_shaped_object(&c.obj, &c.pred);
                let problem = entity_token_problem(&c.subj)
                    .or_else(|| entity_token_problem(&c.obj))
                    .or_else(|| {
                        if secret {
                            Some(
                                "looks like a credential, not a fact — keep it in the vault; \
the store is read by every later session"
                                    .to_string(),
                            )
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        if c.pred
                            .chars()
                            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
                        {
                            None
                        } else {
                            Some("relation must be lower snake_case (e.g. works_at)".to_string())
                        }
                    });
                match problem {
                    Some(reason) => {
                        // The drop report is printed and logged; a credential in it would leak
                        // exactly what this rule exists to keep out.
                        let shown = if secret {
                            format!("{} --{}--> <redacted>", c.subj, c.pred)
                        } else {
                            raw.trim().to_string()
                        };
                        dropped.push((shown, reason));
                    }
                    None => facts.push(c),
                }
            }
            Err(reason) => dropped.push((raw.trim().to_string(), reason)),
        }
    }
    (facts, dropped)
}

/// An [`Extractor`] whose extraction step is a caller-supplied model call —
/// bring your own provider (OpenAI, Anthropic, a local server, a test
/// closure). Lemmalog owns the prompt, the response protocol, and
/// memoization by episode id, so an episode is never re-extracted.
///
/// The model is asked to answer in the line protocol `S --rel--> O`
/// (optionally `S --rel[0.8]--> O`). Extraction failures degrade to zero
/// facts rather than poisoning memory.
/// The model call an [`LlmExtractor`] drives: a prompt in, the model's raw
/// reply or an error message out.
type LlmCall = Box<dyn FnMut(&str) -> Result<String, String>>;

pub struct LlmExtractor {
    call: LlmCall,
    default_confidence: f64,
    seen: HashMap<String, Vec<CandidateFact>>,
    pub calls: usize, // observability for tests/metrics
}

pub const EXTRACTION_PROMPT: &str = "\
Extract the factual triples from the episode below. Answer with one triple \
per line in exactly this format, nothing else:\n\
SUBJECT --RELATION[CONFIDENCE]--> OBJECT\n\
CONFIDENCE is a number in [0,1] (omit [CONFIDENCE] for 0.9). RELATION must \
be one of: works_at, manager, likes, job_title, member_of, located_in, \
links. Employment (works at, joined, was hired by, left) is ALWAYS \
works_at - for a job change emit only the NEW employer as a works_at \
triple. Reporting lines (reports to, manager is) are ALWAYS manager, with \
the person as the subject. Use the closest match for anything else; skip \
facts that fit none. SUBJECT and OBJECT must be real entity names exactly \
as written in the episode: NEVER a pronoun or a role word (speaker, user, \
the manager) - always the full name. Output ONLY the triple lines: no \
reasoning, no explanations, no bullets, no questions. Skip opinions and \
small talk.\n\
Episode:\n";

impl LlmExtractor {
    pub fn new<F>(call: F) -> Self
    where
        F: FnMut(&str) -> Result<String, String> + 'static,
    {
        LlmExtractor {
            call: Box::new(call),
            default_confidence: 0.9,
            seen: HashMap::new(),
            calls: 0,
        }
    }
}

impl Extractor for LlmExtractor {
    fn extract(&mut self, episode: &Episode) -> Vec<CandidateFact> {
        if let Some(cached) = self.seen.get(&episode.id) {
            return cached.clone();
        }
        self.calls += 1;
        let prompt = format!("{EXTRACTION_PROMPT}{}", episode.text);
        let out = match (self.call)(&prompt) {
            Ok(response) => parse_protocol(&response, self.default_confidence),
            Err(_) => Vec::new(), // degraded turn: no facts, no poison
        };
        self.seen.insert(episode.id.clone(), out.clone());
        out
    }

    fn stats(&self) -> (usize, usize) {
        (self.calls, 0)
    }
}

/// Outcome of one `observe()` — the agent-visible update report.
#[derive(Debug, Default, Clone)]
pub struct IngestReport {
    pub added: usize,
    pub updated: usize,
    pub noop: usize,
    /// Facts whose `ts` was older than an open edge's `valid_from`. They are
    /// recorded as closed history and never become current, so they neither
    /// displace the newer value nor produce an interval valid never.
    pub historical: usize,
    /// Facts an observe tried to add while a tombstone covered the triple
    /// (br-gx5.91): the close is deliberate, so the sweep must not revive it.
    pub suppressed: usize,
    pub escalations: Vec<String>,
}

/// Agent memory facade: engine + extraction + episodes + escalations.
pub struct AgentMemory<X: Extractor> {
    pub engine: Engine,
    extractor: X,
    episodes: Vec<Episode>,
    escalations: Vec<String>,
    episode_counter: u64,
    /// Epoch of the last completed `maintain()`; `context()` reports
    /// memory changes since then.
    last_turn_epoch: u64,
    extra_rules: String,
    hyp_counter: u64,
}

pub const DEFAULT_RULES: &str = "\
# temporal projection: what is true NOW
current(E,R,O) :- edge(E,R,O,VF,VT,_), now(T), VF =< T, T < VT.
# point-in-time belief: what the store KNEW at time T (a fact counts
# only once its asserted_at has passed, even if it was valid earlier —
# query with T bound, e.g. via ask_deep)
believed(E,R,O,T) :- edge(E,R,O,VF,VT,AT), AT =< T, VF =< T, T < VT.
# curated exclusivity table for the update policy
exclusive(\"works_at\").
";

/// The transaction clock: unix epoch seconds at the instant of the
/// write. `valid_from` carries when a fact WAS true; `asserted_at`
/// carries when the store LEARNED it — a backdated observe must put the
/// backdated time in one and the wall time in the other, or "what did
/// we believe last month" is unanswerable.
pub fn wall_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl<X: Extractor> AgentMemory<X> {
    pub fn new(extractor: X, extra_rules: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let mut engine = Engine::new();
        engine.install_program(DEFAULT_RULES)?;
        if !extra_rules.trim().is_empty() {
            engine.install_program(extra_rules)?;
        }
        Ok(AgentMemory {
            engine,
            extractor,
            episodes: Vec::new(),
            escalations: Vec::new(),
            episode_counter: 0,
            last_turn_epoch: 0,
            extra_rules: extra_rules.to_string(),
            hyp_counter: 0,
        })
    }

    /// Ingest one episode at the current engine time.
    pub fn observe(&mut self, text: &str) -> IngestReport {
        let ts = self.engine.now;
        self.observe_at(text, ts)
    }

    /// Ingest an episode with a known speaker: first-person references
    /// ("I", "my") resolve to `speaker` during extraction.
    pub fn observe_as(&mut self, text: &str, ts: i64, speaker: &str) -> IngestReport {
        self.engine.set_now(ts);
        self.episode_counter += 1;
        let episode = Episode {
            id: format!("ep{}", self.episode_counter),
            text: text.to_string(),
            ts,
            speaker: Some(speaker.to_string()),
        };
        let candidates = self.extractor.extract(&episode);
        let mut report = IngestReport::default();
        for c in &candidates {
            self.apply_update(c, &episode, &mut report);
        }
        self.episodes.push(episode);
        self.escalations.extend(report.escalations.clone());
        report
    }

    /// Ingest one episode at an explicit timestamp: the extraction boundary
    /// is bi-temporal — `ts` becomes both valid-from of new facts and the
    /// closing valid-to of facts they supersede. Call `maintain()` (the
    /// sleep-time slot) afterwards to re-derive.
    pub fn observe_at(&mut self, text: &str, ts: i64) -> IngestReport {
        self.engine.set_now(ts);
        self.episode_counter += 1;
        let episode = Episode {
            id: format!("ep{}", self.episode_counter),
            text: text.to_string(),
            ts,
            speaker: None,
        };
        let candidates = self.extractor.extract(&episode);
        let mut report = IngestReport::default();
        for c in &candidates {
            self.apply_update(c, &episode, &mut report);
        }
        self.episodes.push(episode);
        self.escalations.extend(report.escalations.clone());
        report
    }

    /// Deterministic update decision for one candidate fact:
    /// - no open fact with same (S,P)      -> ADD
    /// - open fact with same (S,P,O)       -> NOOP (annotation merge)
    /// - open fact with different O:
    ///     - P exclusive                   -> UPDATE (close old, assert new)
    ///       a fact older than an open edge's valid_from is HISTORY: it never
    ///       closes that edge and never becomes current (`report.historical`)
    ///     - otherwise                     -> ADD + escalation
    fn apply_update(&mut self, c: &CandidateFact, ep: &Episode, report: &mut IngestReport) {
        let subj = self.engine.sym(&c.subj);
        let pred = self.engine.sym(&c.pred);
        // digit-only objects become integers: quantities and amounts
        // asserted through the line protocol must feed `sum`/`count`
        // aggregates and `<`/`>` comparisons, which symbols cannot.
        // Mixed-form tokens (dates "2023-05-14", "$50", ids with dashes)
        // stay symbols.
        let obj = match c.obj.parse::<i64>() {
            Ok(n) => Value::Int(n),
            Err(_) => self.engine.sym(&c.obj),
        };
        // br-gx5.91: a tombstoned triple stays closed until `reopen` lifts it. Without this
        // gate the open.is_empty() branch below re-adds poison on the very next sweep.
        if !self
            .engine
            .query("tombstone", &[Some(subj), Some(pred), Some(obj), None])
            .is_empty()
        {
            report.suppressed += 1;
            return;
        }
        let open: Vec<Vec<Value>> = self
            .engine
            .query("edge", &[Some(subj), Some(pred), None, None, None, None])
            .into_iter()
            .map(|(k, _)| k)
            .filter(|k| matches!(k[4].as_int(), Some(vt) if vt == i64::MAX))
            .collect();
        if open.is_empty() {
            self.assert_open(&[subj, pred, obj], c.confidence, &ep.id);
            report.added += 1;
            return;
        }
        if open.iter().any(|k| k[2] == obj) {
            // same fact re-observed: merge annotation, no structural change
            let mut k = open[0].clone();
            k[2] = obj;
            self.engine
                .declare("edge", &k, Ann::base(c.confidence, [ep.id.clone()]));
            report.noop += 1;
            return;
        }
        // relation semantics by name: naturally multi-valued relations
        // accumulate silently (escalating on every `evidence` add taught
        // agents to fight the store); naturally functional ones
        // supersede without needing an explicit `exclusive()` declare
        // (without this, status(H, supported) left status(H, proposed)
        // open too — both "true" at once)
        // Multi-valued relations (policy: skills/software-development/lemmalog/
        // references/relation-policy.md). A subject legitimately holds many of these, so a
        // new value must not close the previous one. `path` and `sha` were being declared
        // exclusive per store, which made 171 of 172 paths look superseded.
        const MULTI: [&str; 18] = [
            "evidence",
            "mentions",
            "located",
            "describes",
            "tag",
            "related_to",
            "depends_on",
            "owns",
            "calls",
            "part_of",
            "source",
            "cites",
            "symptom_of",
            "aka",
            "alias_of",
            "path",
            "sha",
            "note",
        ];
        const FUNCTIONAL: [&str; 7] = [
            "status",
            "phone",
            "address",
            "email",
            "version",
            "value_of",
            "current_value",
        ];
        let pred_name = self.engine.interner.display(&pred).to_string();
        let multi = MULTI.iter().any(|m| pred_name.starts_with(m));
        let functional = FUNCTIONAL.iter().any(|f| pred_name.starts_with(f));
        let exclusive = functional || !self.engine.query("exclusive", &[Some(pred)]).is_empty();
        if exclusive && !multi {
            // An open edge whose valid_from is LATER than this fact's ts must
            // not be closed by it (br-gx5.13). Closing it wrote
            // VF=2000,VT=1500 — an interval valid never — and the newer value
            // was unrecoverable, because `retract` only matches open rows.
            // Such a fact is history the caller is filling in: it closes the
            // rows that started at or before it, takes a closed interval that
            // ends where the next one begins, and never becomes current.
            let now = self.engine.now;
            let (newer, older): (Vec<&Vec<Value>>, Vec<&Vec<Value>>) = open
                .iter()
                .partition(|k| matches!(k[3].as_int(), Some(vf) if vf > now));
            for old in older {
                let mut closed = old.clone();
                closed[4] = Value::Int(now);
                self.engine.retract("edge", old);
                self.engine
                    .declare("edge", &closed, Ann::base(0.9, ["superseded"]));
            }
            if let Some(end) = newer.iter().filter_map(|k| k[3].as_int()).min() {
                // newer is non-empty and every row in it carries an integer
                // valid_from, so `end > now` and the interval cannot invert
                self.assert_historical(&[subj, pred, obj], end, c.confidence, &ep.id);
                report.historical += 1;
            } else {
                self.assert_open(&[subj, pred, obj], c.confidence, &ep.id);
                report.updated += 1;
            }
        } else if multi {
            self.assert_open(&[subj, pred, obj], c.confidence, &ep.id);
            report.added += 1;
        } else {
            self.assert_open(&[subj, pred, obj], c.confidence, &ep.id);
            let others: Vec<String> = open
                .iter()
                .map(|k| self.engine.interner.display(&k[2]))
                .collect();
            report.escalations.push(format!(
                "conflict: {} --{}--> {} asserted in {}, but {} also open ({})",
                c.subj,
                c.pred,
                c.obj,
                ep.id,
                c.pred,
                others.join(", ")
            ));
            report.added += 1;
        }
    }

    fn assert_open(&mut self, spo: &[Value; 3], conf: f64, prov: &str) {
        let args = vec![
            spo[0],
            spo[1],
            spo[2],
            Value::Int(self.engine.now), // valid_from: when it was true
            Value::Int(i64::MAX),
            Value::Int(wall_seconds()),   // asserted_at: when we learned it
        ];
        self.engine.declare("edge", &args, Ann::base(conf, [prov]));
    }

    /// Record a fact that arrived after a newer one as closed history
    /// `[ts, end)`. `end` is the valid_from of the next value the store
    /// already holds, so the interval is the bi-temporal truth of an
    /// out-of-order observation: it never becomes current, cannot displace
    /// the newer value, and cannot invert (`end > ts` by construction).
    fn assert_historical(&mut self, spo: &[Value; 3], end: i64, conf: f64, prov: &str) {
        let args = vec![
            spo[0],
            spo[1],
            spo[2],
            Value::Int(self.engine.now), // valid_from: when it was true
            Value::Int(end),             // valid_to: when the next value took over
            Value::Int(wall_seconds()),  // asserted_at: when we learned it
        ];
        self.engine.declare("edge", &args, Ann::base(conf, [prov]));
    }

    /// Advance time and run incremental maintenance (the sleep-time slot).
    /// The epoch the run logs under is remembered so the next `context()`
    /// can report this turn's changes ("what's new in memory").
    pub fn maintain(&mut self, now: i64) -> usize {
        self.engine.set_now(now);
        self.last_turn_epoch = self.engine.epoch();
        self.engine.run()
    }

    pub fn escalations(&self) -> &[String] {
        &self.escalations
    }

    /// Dismiss an escalation (agent resolved it out-of-band).
    pub fn resolve_escalation(&mut self, idx: usize) {
        if idx < self.escalations.len() {
            self.escalations.remove(idx);
        }
    }

    /// Agent-facing read-only query: bindings for an atom like
    /// `current("alice", R, O)` against materialized relations.
    pub fn ask(&self, goal: &str) -> Result<Vec<String>, crate::ast::ParseError> {
        self.engine.ask(goal)
    }

    /// Truth value of a fully-ground goal (`None` when the goal has variables):
    /// see `Engine::holds` — the yes/no surface the CLI and MCP print instead
    /// of an empty row.
    pub fn holds(&self, goal: &str) -> Result<Option<bool>, crate::ast::ParseError> {
        self.engine.holds(goal)
    }

    /// FORK ADDITION: one line explaining an absent `current` answer — see
    /// `Engine::history_hint`. `None` when there is nothing to explain.
    pub fn history_hint(&self, goal: &str) -> Option<String> {
        self.engine.history_hint(goal)
    }

    /// Ingest PRE-PARSED facts (callers that extract themselves, e.g. the
    /// MCP server where the host model does extraction): applies the same
    /// update policy as `observe_at`, and returns the drop report for any
    /// lines the caller's protocol parse rejected.
    pub fn observe_extracted(
        &mut self,
        text: &str,
        ts: i64,
    ) -> (IngestReport, Vec<(String, String)>) {
        self.engine.set_now(ts);
        let (candidates, dropped) = parse_protocol_reported(text, 0.9);
        self.episode_counter += 1;
        let episode = Episode {
            id: format!("ep{}", self.episode_counter),
            text: text.to_string(),
            ts,
            speaker: None,
        };
        let mut report = IngestReport::default();
        for c in &candidates {
            self.apply_update(c, &episode, &mut report);
        }
        self.episodes.push(episode);
        self.escalations.extend(report.escalations.clone());
        (report, dropped)
    }

    /// Agent tool surface: install a rule batch (versioned, revertable).
    pub fn install_rules(&mut self, src: &str) -> Result<String, Box<dyn std::error::Error>> {
        // Empty text used to install a batch that derives nothing, indistinguishable in the
        // batch list from a real one (measured 2026-11-22). Refused HERE rather than in the CLI
        // so the MCP tool cannot do what the CLI refuses.
        if src.trim().is_empty() {
            return Err("nothing to install (empty rule text)".into());
        }
        self.engine.install_program(src)
    }

    /// The id of an installed batch whose text is identical (after trimming), if any.
    ///
    /// Re-installing the SAME text used to create a second batch and warn that the definitions
    /// union; identical text derives identical facts, so a retry must be a no-op. Shared by the
    /// CLI and the MCP tool — a guard in one surface only is how the two drift apart.
    pub fn batch_with_text(&self, src: &str) -> Option<String> {
        let wanted = src.trim();
        self.rule_batches()
            .into_iter()
            .find(|(_, text)| text.trim() == wanted)
            .map(|(id, _)| id)
    }

    /// Predicates this batch (co-)defines that another installed batch
    /// ALSO defines. Datalog union semantics keep both rules active, so
    /// installing a "corrected" rule without uninstalling the old one
    /// silently preserves the old derivations — this makes the
    /// shadow-definition visible at install time instead.
    pub fn batch_conflicts(&self, id: &str) -> Vec<String> {
        let batches = &self.engine.rule_batches;
        let Some(pos) = batches.iter().position(|(b, _, _)| b == id) else {
            return Vec::new();
        };
        // clause ranges: [prev_end, end) per batch (0 for the first)
        let ends: Vec<usize> = batches.iter().map(|(_, _, e)| *e).collect();
        let lo = |i: usize| -> usize {
            if i == 0 {
                0
            } else {
                ends[i - 1]
            }
        };
        let head_preds = |rng: std::ops::Range<usize>| -> Vec<String> {
            self.engine.clauses[rng]
                .iter()
                .filter(|c| !c.is_fact)
                .map(|c| c.head.pred.clone())
                .collect()
        };
        let mine: Vec<String> = head_preds(lo(pos)..ends[pos]);
        let mut out = Vec::new();
        for p in &mine {
            let co_definers: Vec<String> = batches
                .iter()
                .enumerate()
                .filter(|(i, (b, _, _))| {
                    *i != pos && *b != id && head_preds(lo(*i)..ends[*i]).contains(p)
                })
                .map(|(_, (b, _, _))| b.clone())
                .collect();
            if !co_definers.is_empty() {
                out.push(format!(
                    "{p} is also defined by batch(es) {} — the definitions UNION; uninstall those if this was a replacement",
                    co_definers.join(", ")
                ));
            }
        }
        out
    }

    /// Agent tool surface: uninstall a rule batch; derivations revert on
    /// the next `maintain()`.
    pub fn uninstall_rules(&mut self, id: &str) -> bool {
        self.engine.uninstall(id)
    }

    pub fn rule_batches(&self) -> Vec<(String, String)> {
        self.engine.batches()
    }

    /// Lookahead: "what would follow if this episode were true?" Extracts
    /// the episode's candidates (memoized under a hypothetical id, never
    /// colliding with real episodes), evaluates the goal under those
    /// temporary facts, and restores the memory untouched. Returns the
    /// goal bindings and the number of facts the assumption would add.
    pub fn what_if(
        &mut self,
        text: &str,
        goal: &str,
    ) -> Result<(Vec<String>, usize), Box<dyn std::error::Error>> {
        self.hyp_counter += 1;
        let episode = Episode {
            id: format!("hyp{}", self.hyp_counter),
            text: text.to_string(),
            ts: self.engine.now,
            speaker: None,
        };
        let candidates = self.extractor.extract(&episode);
        let now = self.engine.now;
        let extras: Vec<(String, Vec<Value>)> = candidates
            .iter()
            .map(|c| {
                let obj = match c.obj.parse::<i64>() {
                    Ok(n) => Value::Int(n),
                    Err(_) => self.engine.sym(&c.obj),
                };
                (
                    "edge".to_string(),
                    vec![
                        self.engine.sym(&c.subj),
                        self.engine.sym(&c.pred),
                        obj,
                        Value::Int(now),   // valid_from
                        Value::Int(i64::MAX),
                        Value::Int(wall_seconds()), // asserted_at
                    ],
                )
            })
            .collect();
        let refs: Vec<(&str, &[Value])> = extras
            .iter()
            .map(|(p, a)| (p.as_str(), a.as_slice()))
            .collect();
        let rows = self.engine.hypothetical(&refs, goal)?;
        Ok((rows, self.engine.last_hypothetical_facts))
    }

    /// Query `near` relevance facts for a session/entity pair.
    pub fn query_near(
        &self,
        session: crate::intern::Value,
        entity: crate::intern::Value,
    ) -> Vec<(Vec<crate::intern::Value>, crate::eval::Ann)> {
        self.engine
            .query("near", &[Some(session), Some(entity), None])
    }

    /// Demand-driven query (magic sets): answers without materializing the
    /// full fixpoint of the queried predicate. Runs an (idle-cheap)
    /// maintenance pass first so all-free adornments can alias fresh
    /// materialized relations instead of re-deriving closures.
    pub fn ask_deep(&mut self, goal: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let now = self.engine.now;
        self.maintain(now);
        self.engine.ask_deep(goal)
    }

    pub fn why(&self, fact: &str) -> String {
        match crate::ast::parse_program(&format!("{fact}.")) {
            Ok(clauses) if clauses.len() == 1 => {
                let head = &clauses[0].head;
                match self.engine.ground_values(&head.args) {
                    Some(args) => self.engine.why(&head.pred, &args),
                    None => {
                        // Two different causes shared one message: a variable in the goal
                        // (`current(A, R, O)`) and an entity the store has never seen
                        // (`current(a, status, absentvalue)`). Measured 2026-11-22 — the second
                        // was reported as "contains variables", which is simply untrue and sends
                        // the reader looking at their query syntax instead of their data.
                        let mut unknown = Vec::new();
                        let mut has_var = false;
                        for t in &head.args {
                            match t {
                                Term::Sym(s) if self.engine.interner.lookup(s).is_none() => {
                                    unknown.push(s.clone())
                                }
                                Term::Var(_) | Term::Wildcard => has_var = true,
                                _ => {}
                            }
                        }
                        if !unknown.is_empty() {
                            format!(
                                "why: no such fact — not in the store: {}",
                                unknown.join(", ")
                            )
                        } else if has_var {
                            format!("why: {fact} contains variables")
                        } else {
                            format!("why: {fact} is not in the store")
                        }
                    }
                }
            }
            _ => format!("why: cannot parse {fact:?}"),
        }
    }

    pub fn episodes(&self) -> &[Episode] {
        &self.episodes
    }

    /// Extractor observability: (model calls, failures).
    pub fn extractor_stats(&self) -> (usize, usize) {
        self.extractor.stats()
    }

    /// Assemble the context window for a query mentioning `entities`:
    /// distilled facts first, verbatim provenance last, under budget. A
    /// leading "what changed in memory" section reports facts created since
    /// the last `maintain()` (capped).
    pub fn context(&self, entities: &[&str], budget_tokens: usize) -> String {
        let news: Vec<String> = self
            .engine
            .changes_from(self.last_turn_epoch)
            .iter()
            .take(20)
            .map(|(p, a)| self.engine.render_fact(p, a))
            .collect();
        assemble_context(&self.engine, &self.episodes, entities, budget_tokens, &news)
    }

    /// Query-driven context assembly via hybrid retrieval: BM25 over facts
    /// and episodes + entity-match boosting (a query naming an entity pulls
    /// that entity's facts and one-hop neighbors), budget-aware, distilled
    /// facts first and their provenance episodes last. This is the
    /// "selection, not extraction" answer to context bloat.
    pub fn context_for_query(&self, query: &str, budget_tokens: usize) -> String {
        let r = crate::retrieval::Retrieval::build(&self.engine, &self.episodes);
        let sel = r.select(query, budget_tokens);
        r.render(&sel)
    }

    /// Close an open edge instead of deleting it: `valid_to = now` plus a
    /// `tombstone(S, R, O, closed_at)` base fact that gates re-assertion
    /// (br-gx5.91). A mistaken close stays recoverable — the row keeps its
    /// interval and `reopen` brings it back — which `retract`, by decision,
    /// deliberately does not promise.
    pub fn close_facts(&mut self, text: &str) -> (Vec<String>, Vec<String>, Vec<String>) {
        let candidates = parse_protocol_strict(text, 0.9);
        let now = self.engine.now;
        let mut done = Vec::new();
        let mut missing = Vec::new();
        for c in &candidates {
            let subj = self.engine.sym(&c.subj);
            let pred = self.engine.sym(&c.pred);
            let obj = match c.obj.parse::<i64>() {
                Ok(n) => Value::Int(n),
                Err(_) => self.engine.sym(&c.obj),
            };
            let open: Vec<Vec<Value>> = self
                .engine
                .query("edge", &[Some(subj), Some(pred), Some(obj), None, None, None])
                .into_iter()
                .map(|(k, _)| k)
                .filter(|k| matches!(k[4].as_int(), Some(vt) if vt == i64::MAX))
                .collect();
            if open.is_empty() {
                missing.push(format!("{} --{}--> {}", c.subj, c.pred, c.obj));
                continue;
            }
            for row in &open {
                let mut closed = row.clone();
                closed[4] = Value::Int(now);
                self.engine.retract("edge", row);
                self.engine.declare("edge", &closed, Ann::base(0.9, ["closed"]));
            }
            let tomb = vec![subj, pred, obj, Value::Int(now)];
            self.engine.declare("tombstone", &tomb, Ann::base(0.95, ["closed"]));
            done.push(format!("{} --{}--> {}", c.subj, c.pred, c.obj));
        }
        if done.is_empty() {
            return (done, missing, Vec::new());
        }
        let derived_preds: Vec<String> = self.engine.ever_derived.iter().cloned().collect();
        let mut before: std::collections::BTreeMap<String, std::collections::BTreeSet<Vec<Value>>> =
            std::collections::BTreeMap::new();
        for p in &derived_preds {
            before.insert(p.clone(), self.engine.relation_keys(p).into_iter().collect());
        }
        self.maintain(now);
        let mut died = Vec::new();
        for (p, old_keys) in &before {
            let now_keys: std::collections::BTreeSet<Vec<Value>> =
                self.engine.relation_keys(p).into_iter().collect();
            for k in old_keys.difference(&now_keys) {
                died.push(self.engine.render_fact(p, k));
            }
        }
        (done, missing, died)
    }

    /// Lift the tombstone covering (S, R, O) and re-assert the edge as open.
    pub fn reopen_facts(&mut self, text: &str) -> (Vec<String>, Vec<String>) {
        let candidates = parse_protocol_strict(text, 0.9);
        let now = self.engine.now;
        let mut done = Vec::new();
        let mut missing = Vec::new();
        for c in &candidates {
            let subj = self.engine.sym(&c.subj);
            let pred = self.engine.sym(&c.pred);
            let obj = match c.obj.parse::<i64>() {
                Ok(n) => Value::Int(n),
                Err(_) => self.engine.sym(&c.obj),
            };
            let tombs: Vec<Vec<Value>> = self
                .engine
                .query("tombstone", &[Some(subj), Some(pred), Some(obj), None])
                .into_iter()
                .map(|(k, _)| k)
                .collect();
            if tombs.is_empty() {
                missing.push(format!("{} --{}--> {}", c.subj, c.pred, c.obj));
                continue;
            }
            for row in &tombs {
                self.engine.retract("tombstone", row);
            }
            let still_open = self
                .engine
                .query("edge", &[Some(subj), Some(pred), Some(obj), None, None, None])
                .into_iter()
                .map(|(k, _)| k)
                .filter(|k| matches!(k[4].as_int(), Some(vt) if vt == i64::MAX))
                .count();
            if still_open == 0 {
                self.assert_open(&[subj, pred, obj], 0.9, "reopen");
            }
            done.push(format!("{} --{}--> {}", c.subj, c.pred, c.obj));
        }
        self.maintain(now);
        (done, missing)
    }

    /// Retract base facts given in the line protocol. Each open `edge`
    pub fn retract_facts(&mut self, text: &str) -> (Vec<String>, Vec<String>, Vec<String>) {
        let candidates = parse_protocol_strict(text, 0.9);
        let mut done = Vec::new();
        let mut missing = Vec::new();
        for c in &candidates {
            let subj = self.engine.sym(&c.subj);
            let pred = self.engine.sym(&c.pred);
            let obj = match c.obj.parse::<i64>() {
                Ok(n) => Value::Int(n),
                Err(_) => self.engine.sym(&c.obj),
            };
            let open: Vec<Vec<Value>> = self
                .engine
                .query(
                    "edge",
                    &[Some(subj), Some(pred), Some(obj), None, None, None],
                )
                .into_iter()
                .map(|(k, _)| k)
                .filter(|k| matches!(k[4].as_int(), Some(vt) if vt == i64::MAX))
                .collect();
            if open.is_empty() {
                missing.push(format!("{} --{}--> {}", c.subj, c.pred, c.obj));
                continue;
            }
            for row in &open {
                self.engine.retract("edge", row);
            }
            done.push(format!("{} --{}--> {}", c.subj, c.pred, c.obj));
        }
        if done.is_empty() {
            return (done, missing, Vec::new());
        }
        // consequence report: snapshot derived relations before the
        // recompute, diff after — scoped recompute logs predicate-level
        // clears, so per-row feed events don't tell the story
        let derived_preds: Vec<String> = self.engine.ever_derived.iter().cloned().collect();
        let mut before: std::collections::BTreeMap<String, std::collections::BTreeSet<Vec<Value>>> =
            std::collections::BTreeMap::new();
        for p in &derived_preds {
            before.insert(
                p.clone(),
                self.engine.relation_keys(p).into_iter().collect(),
            );
        }
        let now = self.engine.now;
        self.maintain(now);
        let mut died = Vec::new();
        for (p, old_keys) in &before {
            let now_keys: std::collections::BTreeSet<Vec<Value>> =
                self.engine.relation_keys(p).into_iter().collect();
            for k in old_keys.difference(&now_keys) {
                died.push(self.engine.render_fact(p, k));
            }
        }
        (done, missing, died)
    }

    /// `context_for_query` plus the misattribution and supersession
    /// sections: an ATTRIBUTION contrast (which subjects hold facts on
    /// the question's topic — zero counts are the false-premise signal)
    /// and, for current-state/value questions, the newest value per
    /// slot with supersessions listed as history. Same facts, but the
    /// reader cannot confuse Caroline's gift with Melanie's.
    pub fn context_for_query_rich(&self, query: &str, budget_tokens: usize) -> String {
        let mut base = self.context_for_query(query, budget_tokens);
        let qt = crate::retrieval::tokens3(query);
        // attribution contrast
        {
            let mut holders: std::collections::BTreeMap<String, usize> =
                std::collections::BTreeMap::new();
            let mut subjects: Vec<String> = Vec::new();
            for key in self.engine.relation_keys("current") {
                if key.len() != 3 {
                    continue;
                }
                let subj = self.engine.interner.display(&key[0]).to_string();
                if !subjects.contains(&subj) {
                    subjects.push(subj.clone());
                }
                let line = format!(
                    "{} --{}--> {}",
                    subj,
                    self.engine.interner.display(&key[1]),
                    self.engine.interner.display(&key[2])
                );
                if crate::retrieval::topic_overlap(&crate::retrieval::tokens3(&line), &subj, &qt)
                    >= 1
                {
                    *holders.entry(subj).or_insert(0) += 1;
                }
            }
            let asked: Vec<String> = subjects
                .iter()
                .filter(|s| {
                    crate::retrieval::tokens3(s)
                        .iter()
                        .any(|t| t.len() >= 3 && qt.contains(t))
                })
                .cloned()
                .collect();
            let zero: Vec<String> = asked
                .iter()
                .filter(|s| !holders.contains_key(*s))
                .cloned()
                .collect();
            if !holders.is_empty() || !zero.is_empty() {
                let mut sec = String::from(
                    "\nATTRIBUTION (who holds facts on this question's topic, by subject):\n",
                );
                let mut pairs: Vec<_> = holders.iter().collect();
                pairs.sort_by(|a, b| b.1.cmp(a.1));
                for (s, n) in pairs.iter().take(6) {
                    sec.push_str(&format!("  {s}: {n} topic facts\n"));
                }
                if !zero.is_empty() {
                    sec.push_str(&format!(
                        "  NO topic facts for: {} (mentioned in the question)\n",
                        zero.join(", ")
                    ));
                }
                base.push_str(&sec);
            }
        }
        // latest values per slot (supersession history)
        let q_lower = query.to_lowercase();
        if q_lower.contains("current")
            || q_lower.contains(" now")
            || q_lower.contains("latest")
            || q_lower.contains("amount")
            || q_lower.contains("price")
            || q_lower.contains("how much")
            || q_lower.contains("value")
        {
            let mut slots: std::collections::BTreeMap<(String, String), Vec<(i64, String)>> =
                std::collections::BTreeMap::new();
            for key in self.engine.relation_keys("edge") {
                if key.len() != 6 {
                    continue;
                }
                let subj = self.engine.interner.display(&key[0]).to_string();
                let rel = self.engine.interner.display(&key[1]).to_string();
                let line = format!(
                    "{subj} --{rel}--> {}",
                    self.engine.interner.display(&key[2])
                );
                if !crate::retrieval::tokens3(&line)
                    .iter()
                    .any(|t| t.len() >= 3 && qt.contains(t))
                {
                    continue;
                }
                let vf = key[3].as_int().unwrap_or(0);
                if vf == i64::MAX {
                    continue;
                }
                slots
                    .entry((subj, rel))
                    .or_default()
                    .push((vf, self.engine.interner.display(&key[2]).to_string()));
            }
            let mut sec = String::from(
                "\nCURRENT STATE (latest value per slot; superseded values listed as history):\n",
            );
            let mut lines = 0;
            for ((subj, rel), mut vals) in slots {
                if lines >= 8 {
                    break;
                }
                vals.sort_by_key(|v| Reverse(v.0));
                let distinct: Vec<String> = vals.iter().map(|(_, v)| v.clone()).fold(
                    Vec::new(),
                    |mut acc: Vec<String>, v| {
                        if !acc.contains(&v) {
                            acc.push(v);
                        }
                        acc
                    },
                );
                if distinct.len() < 2 {
                    continue;
                }
                let older: Vec<String> = distinct.iter().skip(1).take(3).cloned().collect();
                sec.push_str(&format!(
                    "  {subj} --{rel}--> {} (current; superseded: {})\n",
                    distinct[0],
                    older.join(", ")
                ));
                lines += 1;
            }
            if lines > 0 {
                base.push_str(&sec);
            }
        }
        base
    }
}

// ------------------------------------------------------ context assembler

/// Positional assembly (lost-in-the-middle mitigation): derived high-value
/// facts at the top of the window, verbatim source episodes at the bottom,
/// byte budget `tokens * 4` split 60/40.
pub fn assemble_context(
    engine: &Engine,
    episodes: &[Episode],
    entities: &[&str],
    budget_tokens: usize,
    news: &[String],
) -> String {
    let mut relevant: Vec<(Vec<Value>, Ann)> = Vec::new();
    for name in entities {
        let v = engine.sym_of(name);
        relevant.extend(engine.query("current", &[Some(v), None, None]));
    }
    relevant.sort_by(|a, b| {
        b.1.conf
            .partial_cmp(&a.1.conf)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let distilled_budget = budget_tokens * 4 * 6 / 10;
    let mut distilled = String::new();
    let mut used_prov: Vec<String> = Vec::new();
    for (k, ann) in &relevant {
        let line = format!(
            "{} --{}--> {}   [conf {:.2}, prov {}]\n",
            engine.interner.display(&k[0]),
            engine.interner.display(&k[1]),
            engine.interner.display(&k[2]),
            ann.conf,
            ann.prov.iter().cloned().collect::<Vec<_>>().join(",")
        );
        if distilled.len() + line.len() > distilled_budget {
            break;
        }
        distilled.push_str(&line);
        used_prov.extend(ann.prov.iter().cloned());
    }

    let source_budget = budget_tokens * 4 * 4 / 10;
    let mut sources = String::new();
    let mut used: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for ep in episodes {
        if !used_prov.iter().any(|p| p == &ep.id) {
            continue;
        }
        used.insert(&ep.id);
        let block = format!("[{}] {}\n", ep.id, ep.text);
        if sources.len() + block.len() > source_budget {
            break;
        }
        sources.push_str(&block);
    }
    let _ = used;

    let mut out = String::new();
    if !news.is_empty() {
        let _ = writeln!(out, "== new in memory since last turn ==");
        for line in news {
            let _ = writeln!(out, "{line}");
        }
        let _ = writeln!(out);
    }
    let _ = writeln!(out, "== memory (distilled, highest confidence first) ==");
    out.push_str(&distilled);
    let _ = writeln!(out, "\n== source episodes (verbatim, from provenance) ==");
    out.push_str(&sources);
    out
}

impl Engine {
    /// Non-mutating symbol lookup for read-only paths.
    pub fn sym_of(&self, s: &str) -> Value {
        match self.interner.lookup(s) {
            Some(v) => Value::Sym(v),
            None => Value::Int(i64::MIN), // never matches: unknown entity
        }
    }

    /// Resolve a pattern to ground values for `why()` (None if vars remain).
    pub fn ground_values(&self, pat: &[Term]) -> Option<Vec<Value>> {
        pat.iter()
            .map(|t| match t {
                Term::Sym(s) => self.interner.lookup(s).map(Value::Sym),
                Term::Int(i) => Some(Value::Int(*i)),
                _ => None,
            })
            .collect()
    }
}

// ------------------------------------------------------------- persistence

/// Argument representation while parsing a snapshot (symbols intern
/// against the target engine once it exists).
enum ArgRepr {
    S(String),
    I(i64),
}

/// Escape a field for the tab-separated snapshot format.
fn esc(s: &str) -> String {
    // spaces too: snapshot fields and fact args are space-separated, so
    // multi-word symbols (extracted entities like "United Airlines") must
    // not split on reload
    s.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace(' ', "\\s")
}

fn unesc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('t') => out.push('\t'),
                Some('n') => out.push('\n'),
                Some('s') => out.push(' '),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

const SNAPSHOT_MAGIC: &str = "LEMMALOG1";
/// The batch `new()` installs from `DEFAULT_RULES`; never persisted.
const BOOTSTRAP_BATCH: &str = "b0";
/// Pre-rename snapshots (read-only compatibility).
const SNAPSHOT_MAGIC_V0: &str = "CORTEXLOG1";

impl<X: Extractor> AgentMemory<X> {
    /// Persist to a snapshot file: rules, clock, episodes (verbatim
    /// sources), escalation queue, and all base (EDB) facts with their
    /// annotations. Derived relations are NOT persisted — they are
    /// rebuildable projections, recomputed by `load()`.
    pub fn save(&self, path: &str) -> std::io::Result<()> {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(out, "{SNAPSHOT_MAGIC}");
        let _ = writeln!(out, "NOW\t{}", self.engine.now);
        let _ = writeln!(out, "RULES\t{}", esc(&self.extra_rules));
        // Batches installed after construction (`install_rules`) are not in
        // `extra_rules`, so persisting only that field drops every rule an
        // agent installed — silently, since base facts still load and the
        // derived views simply come back empty. Batch ids are positional
        // (`b{len}`), so replaying non-bootstrap batches in order restores
        // them under the same ids and `uninstall` keeps working.
        for (id, src) in self.engine.batches() {
            if id == BOOTSTRAP_BATCH {
                continue; // reinstalled by `new()` from DEFAULT_RULES
            }
            let _ = writeln!(out, "RULEB\t{}\t{}", esc(&id), esc(&src));
        }
        for ep in &self.episodes {
            let _ = writeln!(
                out,
                "EP\t{}\t{}\t{}\t{}",
                esc(&ep.id),
                ep.ts,
                ep.speaker.as_deref().unwrap_or(""),
                esc(&ep.text)
            );
        }
        for e in &self.escalations {
            let _ = writeln!(out, "ESC\t{}", esc(e));
        }
        for (pred, rel) in &self.engine.relations {
            // base facts only: skip predicates defined by rules (they are
            // either program facts re-declared from rules, or derived)
            if self.engine.clauses.iter().any(|c| c.head.pred == *pred) {
                continue;
            }
            for row in &rel.rows {
                let prov = row
                    .fact
                    .ann
                    .prov
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(",");
                let args = row
                    .key
                    .iter()
                    .map(|v| match v {
                        Value::Sym(s) => format!("s:{}", esc(self.engine.interner.resolve(*s))),
                        Value::Int(i) => format!("i:{i}"),
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                let _ = writeln!(
                    out,
                    "FACT\t{}\t{}\t{}\t{}",
                    pred, row.fact.ann.conf, prov, args
                );
            }
        }
        crate::store::write_atomic(path, &out)
    }

    /// Load a snapshot into a fresh memory with the given extractor.
    /// Base facts are re-asserted with their annotations; derived
    /// relations are rebuilt by one maintenance run.
    pub fn load(extractor: X, path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let text = std::fs::read_to_string(path)?;
        let mut lines = text.lines();
        let magic = lines.next();
        if magic != Some(SNAPSHOT_MAGIC) && magic != Some(SNAPSHOT_MAGIC_V0) {
            return Err("not a lemmalog snapshot".into());
        }
        let mut rules = String::new();
        let mut batch_srcs: Vec<String> = Vec::new();
        let mut now = 0i64;
        let mut episodes = Vec::new();
        let mut escalations = Vec::new();
        let mut facts: Vec<(String, f64, Vec<String>, Vec<ArgRepr>)> = Vec::new();
        for line in lines {
            let Some((tag, rest)) = line.split_once('\t') else {
                continue;
            };
            match tag {
                "NOW" => now = rest.parse()?,
                "RULES" => rules = unesc(rest),
                "RULEB" => {
                    let mut f = rest.splitn(2, '\t');
                    let (Some(_id), Some(src)) = (f.next(), f.next()) else {
                        return Err("bad RULEB record".into());
                    };
                    batch_srcs.push(unesc(src));
                }
                "EP" => {
                    let mut f = rest.splitn(4, '\t');
                    let (Some(id), Some(ts), Some(speaker), Some(txt)) =
                        (f.next(), f.next(), f.next(), f.next())
                    else {
                        return Err("bad EP record".into());
                    };
                    episodes.push(Episode {
                        id: unesc(id),
                        ts: ts.parse()?,
                        speaker: if speaker.is_empty() {
                            None
                        } else {
                            Some(unesc(speaker))
                        },
                        text: unesc(txt),
                    });
                }
                "ESC" => escalations.push(unesc(rest)),
                "FACT" => {
                    let mut f = rest.splitn(4, '\t');
                    let (Some(pred), Some(conf), Some(prov), Some(args)) =
                        (f.next(), f.next(), f.next(), f.next())
                    else {
                        return Err("bad FACT record".into());
                    };
                    let mut vals = Vec::new();
                    for a in args.split(' ').filter(|s| !s.is_empty()) {
                        if let Some(sym) = a.strip_prefix("s:") {
                            vals.push(ArgRepr::S(unesc(sym)));
                        } else if let Some(i) = a.strip_prefix("i:") {
                            vals.push(ArgRepr::I(i.parse()?));
                        } else {
                            return Err(format!("bad arg {a:?}").into());
                        }
                    }
                    let prov: Vec<String> = prov
                        .split(',')
                        .filter(|s| !s.is_empty())
                        .map(unesc)
                        .collect();
                    facts.push((pred.to_string(), conf.parse()?, prov, vals));
                }
                _ => {}
            }
        }
        let mut m = if batch_srcs.is_empty() {
            // Legacy snapshot (or nothing installed past the bootstrap).
            AgentMemory::new(extractor, &rules)?
        } else {
            // `extra_rules` became a batch at construction, so it is already
            // in `batch_srcs`; passing it again would install it twice.
            let mut m = AgentMemory::new(extractor, "")?;
            for src in &batch_srcs {
                m.engine.install_program(src)?;
            }
            m.extra_rules = rules;
            m
        };
        m.escalations = escalations;
        m.episodes = episodes;
        m.episode_counter = m.episodes.len() as u64;
        for (pred, conf, prov, args) in facts {
            let resolved: Vec<Value> = args
                .into_iter()
                .map(|v| match v {
                    ArgRepr::S(name) => m.engine.sym(&name),
                    ArgRepr::I(i) => Value::Int(i),
                })
                .collect();
            m.engine.declare(&pred, &resolved, Ann::base(conf, prov));
        }
        m.engine.set_now(now);
        let _ = m.engine.run();
        m.last_turn_epoch = m.engine.epoch();
        Ok(m)
    }
}


#[cfg(test)]
mod entity_message_tests {
    //! The diagnostic must name the real cause: a wrong reason is a bug even when the drop is
    //! correct, because it sends the operator looking in the wrong place.

    use super::*;

    #[test]
    fn a_long_single_token_is_too_long_not_prose() {
        let long = "x".repeat(5000);
        let msg = entity_token_problem(&long).expect("must be refused");
        assert!(msg.contains("too long"), "{msg}");
        assert!(
            !msg.contains("prose"),
            "5000 characters is ONE word; calling it prose is a false diagnosis: {msg}"
        );
    }

    #[test]
    fn a_many_word_phrase_is_prose() {
        let msg = entity_token_problem("one two three four five six seven eight nine")
            .expect("must be refused");
        assert!(msg.contains("prose"), "{msg}");
        assert!(msg.contains("9 words"), "{msg}");
    }

    #[test]
    fn an_accented_single_token_is_a_name() {
        // The spaced path already accepted `café com leite`; refusing `café` alone made the
        // accent depend on the presence of a space (measured 2026-11-22).
        assert!(entity_token_problem("café").is_none());
        assert!(entity_token_problem("ação").is_none());
        assert!(entity_token_problem("açúcar").is_none());
    }

    #[test]
    fn visible_symbols_are_name_characters() {
        // These are the plant: every one of them was refused by the ASCII rule (13 drops on
        // 2026-11-22). None is prose, none can hide a second value.
        for name in ["270\u{00B0}", "3012\u{2192}3002", "\u{00B1}175", "\u{2264}200ms", "smile\u{263A}"] {
            assert!(
                entity_token_problem(name).is_none(),
                "{name} is visible text and must be storable"
            );
        }
    }

    #[test]
    fn units_and_operators_are_names_not_prose() {
        // The 13 facts the ASCII rule refused on 2026-11-22, verbatim. Prose is signalled by
        // commas and brackets, not by a degree sign.
        for name in [
            "transform 3 = 270\u{00B0}",
            "port 3012\u{2192}3002",
            "crop window \u{00B1}175 chars",
            "latency \u{2264}200 ms",
            "usefulness wobbles \u{00B1}10 points on live DB",
        ] {
            assert!(entity_token_problem(name).is_none(), "{name} must store");
        }
        // and the prose signals stay refused
        for prose in ["modifies no game file, memory or code", "completed normally (exit code 0)"] {
            assert!(entity_token_problem(prose).is_some(), "{prose} must stay refused");
        }
    }

    #[test]
    fn invisible_and_control_characters_are_refused_and_named() {
        for bad in ["zero\u{200B}width", "bidi\u{202E}swap", "nul\u{0}byte"] {
            let msg = entity_token_problem(bad).expect("must be refused");
            assert!(
                msg.contains("control or invisible"),
                "reason must name the real cause, got: {msg}"
            );
        }
    }

    #[test]
    fn a_real_control_character_is_still_refused() {
        assert!(entity_token_problem("a\u{0}b").is_some());
    }
}

/// Capitalized words in a goal — the names Datalog reads as variables. Quoted spans are
/// skipped, so a properly quoted entity (`current("Svc", port, P)`) never triggers the hint.
///
/// Both surfaces need this: the store allows capitalized entities (`Svc`, `Sunshine`,
/// `AGENTS.md` — 66 of 724 live subjects), yet a bare capitalized word in a goal binds a
/// VARIABLE and silently returns every subject.
pub fn variable_like_names(goal: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in goal.chars() {
        if c == '"' {
            in_quotes = !in_quotes;
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            continue;
        }
        if in_quotes {
            continue;
        }
        if c.is_alphanumeric() || c == '_' {
            cur.push(c);
        } else if !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
        .into_iter()
        .filter(|w| w.chars().next().map(char::is_uppercase).unwrap_or(false))
        .collect()
}
