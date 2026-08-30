//! Text as vectors, which is a different question from chat.
//!
//! Different request, different reply, different usage shape. No messages, no stop reason,
//! no reasoning, no tools. Almost nothing in [`crate::chat`] applies, which is why this is
//! its own trait rather than a method on [`Provider`](crate::Provider): putting `embed`
//! there would make every chat-only provider implement a refusal.
//!
//! What it *does* share is everything a caller relies on — [`Reach`], [`crate::Error`] and
//! its retry advice, [`Usage`] with its absent-is-not-zero rule, and the transport boundary.
//! An embedding call has a reach, costs money, and can go unmeasured, and every one of those
//! answers should be the same answer it is for chat. `docs/DESIGN.md` records why that
//! argument won over shipping a second crate.
//!
//! # A vector belongs to the model that made it
//!
//! This is the rule the module is built around. Two vectors of the same length from two
//! different models are not comparable, and nothing about them says so: cosine similarity
//! computes happily and returns a number between -1 and 1 that means nothing at all. It is
//! the same failure as adding dollars to euros, and it is caught the same way.
//!
//! So [`Embedding`] carries the model that produced it, and [`Embedding::similarity`]
//! answers `None` rather than a number when asked to compare across models.
//!
//! ```
//! use llmr::embed::{Embedding, Purpose};
//!
//! let one = Embedding::new("text-embedding-3-small".into(), vec![1.0, 0.0]);
//! let same = Embedding::new("text-embedding-3-small".into(), vec![1.0, 0.0]);
//! let other = Embedding::new("some-other-model".into(), vec![1.0, 0.0]);
//!
//! assert_eq!(one.similarity(&same), Some(1.0));
//! assert_eq!(one.similarity(&other), None, "not a comparison anybody can make");
//! ```
//!
//! # Order is the contract
//!
//! [`Embeddings`] comes back index for index with the inputs that produced it. Several
//! vendors return an `index` field precisely because their replies are not ordered, and a
//! provider that trusts arrival order silently pairs every document with the wrong vector.
//! An implementation must sort by that field, and the contract suite checks it.

use crate::cost::usage::Usage;
use crate::model::{ModelId, Reach};
use crate::provider::Access;
use crate::Result;
use async_trait::async_trait;

/// What the text is for, when a model distinguishes.
///
/// Several models produce a different vector for "store this document" than for "search
/// with this query", and the two are meant to be used together: a query vector is built to
/// land near the documents that answer it. Embedding a query as a document is not an error
/// anywhere — it returns vectors, retrieval still runs, and the results are quietly worse.
///
/// Read [`EmbeddingCapabilities::purposes`] to find out whether a pairing distinguishes them
/// at all. A model that does not is not a model this is wrong for; it is one where the
/// answer is the same either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Purpose {
    /// Text being stored, to be found later.
    Document,
    /// Text being searched with, to find stored documents.
    Query,
}

impl Purpose {
    /// How a purpose is written down, in a record or a report.
    pub fn as_str(self) -> &'static str {
        match self {
            Purpose::Document => "document",
            Purpose::Query => "query",
        }
    }
}

impl std::fmt::Display for Purpose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Some text to turn into vectors.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct EmbedRequest {
    /// Which model to ask.
    pub model: ModelId,
    /// The texts, in the order the vectors will come back in.
    pub inputs: Vec<String>,
    /// What the text is for, when the caller knows and the model cares.
    ///
    /// `None` means the caller did not say, and a provider sends nothing rather than
    /// choosing — a guessed purpose is a retrieval quality problem that never shows up as an
    /// error.
    pub purpose: Option<Purpose>,
    /// How many dimensions to ask for, when the model can be asked.
    ///
    /// `None` means the model's own size. Read
    /// [`EmbeddingCapabilities::resizable`] first: a provider that cannot honour this
    /// refuses rather than returning full length vectors a caller has already sized a
    /// database for.
    pub dimensions: Option<u32>,
}

impl EmbedRequest {
    /// A request for one model over some texts.
    ///
    /// This type is marked non exhaustive so fields can be added without breaking your code,
    /// which also means you cannot build one with a struct literal.
    pub fn new(model: ModelId, inputs: Vec<String>) -> Self {
        Self {
            model,
            inputs,
            purpose: None,
            dimensions: None,
        }
    }

    /// A request for one piece of text.
    pub fn one(model: impl Into<ModelId>, input: impl Into<String>) -> Self {
        Self::new(model.into(), vec![input.into()])
    }

    /// Says what the text is for.
    #[must_use]
    pub fn for_purpose(mut self, purpose: Purpose) -> Self {
        self.purpose = Some(purpose);
        self
    }

    /// Asks for a particular number of dimensions.
    #[must_use]
    pub fn with_dimensions(mut self, dimensions: u32) -> Self {
        self.dimensions = Some(dimensions);
        self
    }

    /// How many texts are in here.
    pub fn len(&self) -> usize {
        self.inputs.len()
    }

    /// Whether there is nothing to embed.
    ///
    /// Worth checking before sending: several endpoints answer an empty batch with a 400,
    /// and a loop over an empty collection reaching the network at all is a bug upstream.
    pub fn is_empty(&self) -> bool {
        self.inputs.is_empty()
    }
}

/// One vector, and the model that produced it.
///
/// The model is not decoration. Vectors from two models occupy unrelated spaces, and every
/// operation anybody performs on them — similarity, clustering, a nearest neighbour index —
/// works perfectly and means nothing. Carrying the name is what lets [`Embedding::similarity`]
/// refuse.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Embedding {
    /// Which model produced this vector.
    ///
    /// The model that *served* the call, as the reply said, not the one that was asked for.
    pub model: ModelId,
    /// The vector.
    pub vector: Vec<f32>,
}

impl Embedding {
    /// A vector from a model.
    pub fn new(model: ModelId, vector: Vec<f32>) -> Self {
        Self { model, vector }
    }

    /// How many dimensions it has.
    pub fn dimensions(&self) -> usize {
        self.vector.len()
    }

    /// Cosine similarity with another vector, when the two can be compared at all.
    ///
    /// `None` for the two cases where the number would be meaningless rather than wrong:
    ///
    /// - **Different models.** Two embedding spaces with no relation to each other. This is
    ///   the case worth refusing, because it is the one that produces a perfectly plausible
    ///   number.
    /// - **Different lengths, or either one empty.** No such number exists. A model asked
    ///   for fewer dimensions produces a vector that is not comparable with a full length one
    ///   from the same model either.
    ///
    /// Otherwise a number from -1 to 1. A zero vector gives `Some(0.0)`: it has no direction,
    /// so it points at nothing, and that is the honest answer rather than a division by zero.
    pub fn similarity(&self, other: &Embedding) -> Option<f32> {
        if self.model != other.model {
            return None;
        }
        if self.vector.len() != other.vector.len() || self.vector.is_empty() {
            return None;
        }

        let mut dot = 0.0f32;
        let mut here = 0.0f32;
        let mut there = 0.0f32;
        for (a, b) in self.vector.iter().zip(&other.vector) {
            dot += a * b;
            here += a * a;
            there += b * b;
        }

        let magnitude = here.sqrt() * there.sqrt();
        if magnitude == 0.0 {
            return Some(0.0);
        }
        Some((dot / magnitude).clamp(-1.0, 1.0))
    }
}

/// What came back from one embedding call.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Embeddings {
    /// One vector per input, in the order the inputs were given.
    pub vectors: Vec<Embedding>,
    /// Which model actually served this.
    ///
    /// Can differ from the one asked for, exactly as it can for chat. Price against this
    /// one, and store it beside the vectors: a database of vectors whose model nobody wrote
    /// down cannot be re-embedded when the model is retired.
    pub model: ModelId,
    /// What the call consumed, as far as the provider reported it.
    ///
    /// [`Usage::absent`] when it reported nothing. Not zeros — see [`Usage::embedding`] for
    /// the shape an embedding endpoint reports, which is prompt tokens and nothing else.
    pub usage: Usage,
}

impl Embeddings {
    /// A reply.
    ///
    /// This type is marked non exhaustive so fields can be added without breaking your code,
    /// which also means you cannot build one with a struct literal. This is how a provider
    /// outside this crate builds its answer.
    pub fn new(vectors: Vec<Embedding>, model: ModelId, usage: Usage) -> Self {
        Self {
            vectors,
            model,
            usage,
        }
    }

    /// How many vectors came back.
    pub fn len(&self) -> usize {
        self.vectors.len()
    }

    /// Whether nothing came back.
    pub fn is_empty(&self) -> bool {
        self.vectors.is_empty()
    }

    /// The vector for the input at this position.
    pub fn get(&self, index: usize) -> Option<&Embedding> {
        self.vectors.get(index)
    }

    /// How many dimensions the vectors have, when they agree.
    ///
    /// `None` for an empty reply, and for the pathological case of a batch whose vectors are
    /// not all the same length. The second should never happen and is worth finding out
    /// about rather than averaging over.
    pub fn dimensions(&self) -> Option<usize> {
        let first = self.vectors.first()?.dimensions();
        self.vectors
            .iter()
            .all(|v| v.dimensions() == first)
            .then_some(first)
    }

    /// Just the numbers, when the caller is writing them somewhere that already knows the
    /// model.
    ///
    /// Named rather than a `From`, because dropping the model is the one thing this module
    /// is careful about and it should be something a reader can see happening.
    pub fn into_vectors(self) -> Vec<Vec<f32>> {
        self.vectors.into_iter().map(|e| e.vector).collect()
    }
}

/// What an embedding model can do when reached this way.
///
/// The same bargain [`crate::ModelCapabilities`] makes for chat: ask before you send, rather
/// than find out from a reply that is subtly not what you wanted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct EmbeddingCapabilities {
    /// How many dimensions the vectors have by default.
    ///
    /// Zero means unknown, following [`crate::ModelCapabilities::none`]: a guessed dimension
    /// is a database column of the wrong width, and the caller has no way to know the number
    /// was invented.
    pub dimensions: u32,
    /// The most tokens one input may be.
    pub max_input_tokens: u32,
    /// The most inputs one call may carry.
    ///
    /// Zero means unknown. A caller batching against an unknown limit should batch small.
    pub max_batch: u32,
    /// Whether [`EmbedRequest::dimensions`] is honoured.
    pub resizable: bool,
    /// Whether [`Purpose`] changes the vector.
    ///
    /// False is a real answer, not a missing one: for most models the vector is the same
    /// either way, and a caller who knows that can stop threading the distinction through.
    pub purposes: bool,
    /// Where this pairing runs.
    pub reach: Reach,
}

impl EmbeddingCapabilities {
    /// A capability set with everything off and no size, for a provider to fill in.
    ///
    /// Zeros rather than plausible defaults, for the same reason
    /// [`crate::ModelCapabilities::none`] uses them.
    pub fn none(reach: Reach) -> Self {
        Self {
            dimensions: 0,
            max_input_tokens: 0,
            max_batch: 0,
            resizable: false,
            purposes: false,
            reach,
        }
    }

    /// Says how many dimensions the vectors have.
    #[must_use]
    pub fn with_dimensions(mut self, dimensions: u32) -> Self {
        self.dimensions = dimensions;
        self
    }

    /// Says how long one input may be.
    #[must_use]
    pub fn with_max_input_tokens(mut self, tokens: u32) -> Self {
        self.max_input_tokens = tokens;
        self
    }

    /// Says how many inputs one call may carry.
    #[must_use]
    pub fn with_max_batch(mut self, inputs: u32) -> Self {
        self.max_batch = inputs;
        self
    }

    /// Says the dimension count can be asked for.
    #[must_use]
    pub fn resizable(mut self) -> Self {
        self.resizable = true;
        self
    }

    /// Says [`Purpose`] changes the vector.
    #[must_use]
    pub fn with_purposes(mut self) -> Self {
        self.purposes = true;
        self
    }
}

/// Something that can turn text into vectors.
///
/// # Implementing one
///
/// Four rules, and the first two are the ones that fail quietly.
///
/// 1. **The reply is index for index with the request.** Sort by whatever index field the
///    vendor sends rather than trusting arrival order. Getting this wrong pairs every
///    document with another document's vector, and every later query still returns results.
/// 2. **Every [`Embedding`] carries the model that served it**, from the reply rather than
///    from the request. It is what stops two embedding spaces being compared as one.
/// 3. **Do not invent usage.** [`Usage::absent`] when the provider reported nothing, and
///    [`Usage::embedding`] when it reported prompt tokens — never zeros.
/// 4. **`embed` takes `&self`**, so anything shared must be immutable after construction or
///    behind an atomic, exactly as for [`crate::Provider`].
#[async_trait]
pub trait Embedder: Send + Sync {
    /// A short name for this embedder, used in records and reports.
    fn id(&self) -> &str;

    /// What this model can do when reached through this embedder.
    ///
    /// `None` for a model it does not recognise, which is a different answer from one it
    /// knows and has nothing to offer for.
    fn capabilities(&self, model: &ModelId) -> Option<EmbeddingCapabilities>;

    /// Turns some text into vectors.
    ///
    /// # Errors
    ///
    /// See [`crate::Error`]. A reply the provider sent and this crate could not read is an
    /// [`crate::Error::Unreadable`], never a short list of vectors: a batch that comes back
    /// with fewer vectors than it had inputs cannot be lined up, and guessing which input
    /// was dropped is worse than failing.
    async fn embed(&self, request: EmbedRequest) -> Result<Embeddings>;

    /// Whether a request for this model would be accepted, without sending one.
    ///
    /// The same two rules as [`crate::Provider::validate`]: it must not cost anything, and it
    /// must not report a rejected credential as [`Access::Unknown`]. The default answers
    /// `Unknown`, which is the honest answer for an embedder with nothing free to ask.
    async fn validate(&self, _model: &ModelId) -> Access {
        Access::unknown(format!("{} has no free way to be asked", self.id()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vector(model: &str, numbers: &[f32]) -> Embedding {
        Embedding::new(model.into(), numbers.to_vec())
    }

    #[test]
    fn two_vectors_from_different_models_cannot_be_compared() {
        // The rule this module is built around. Both are unit vectors on the same axis, so
        // the arithmetic would give a confident 1.0 for two things that have nothing to do
        // with each other.
        let one = vector("text-embedding-3-small", &[1.0, 0.0, 0.0]);
        let other = vector("embed-english-v3", &[1.0, 0.0, 0.0]);

        assert_eq!(one.similarity(&other), None);
        assert_eq!(
            one.similarity(&vector("text-embedding-3-small", &[1.0, 0.0, 0.0])),
            Some(1.0),
            "the same model still compares"
        );
    }

    #[test]
    fn vectors_of_different_lengths_have_no_similarity() {
        // Including from the same model, which is what asking for fewer dimensions produces.
        let full = vector("m", &[1.0, 0.0, 0.0]);
        let shortened = vector("m", &[1.0, 0.0]);
        assert_eq!(full.similarity(&shortened), None);
    }

    #[test]
    fn an_empty_vector_compares_to_nothing() {
        assert_eq!(vector("m", &[]).similarity(&vector("m", &[])), None);
    }

    #[test]
    fn a_zero_vector_points_at_nothing_rather_than_dividing_by_nothing() {
        let zero = vector("m", &[0.0, 0.0]);
        assert_eq!(zero.similarity(&vector("m", &[1.0, 0.0])), Some(0.0));
        assert_eq!(zero.similarity(&zero), Some(0.0));
    }

    #[test]
    fn opposite_vectors_are_minus_one_and_perpendicular_ones_are_nought() {
        let east = vector("m", &[1.0, 0.0]);
        let west = vector("m", &[-1.0, 0.0]);
        let north = vector("m", &[0.0, 1.0]);

        assert_eq!(east.similarity(&west), Some(-1.0));
        assert_eq!(east.similarity(&north), Some(0.0));
    }

    #[test]
    fn similarity_never_leaves_the_range_rounding_could_push_it_out_of() {
        // Floating point on a long vector can produce 1.0000001, and a caller computing a
        // distance as `1 - similarity` would get a negative one.
        let long = Embedding::new("m".into(), vec![0.1; 3_000]);
        let same = long.clone();
        let answer = long.similarity(&same).unwrap_or(f32::NAN);
        assert!((-1.0..=1.0).contains(&answer), "{answer}");
    }

    #[test]
    fn a_request_says_nothing_about_purpose_unless_asked_to() {
        // A guessed purpose is a retrieval quality problem that never surfaces as an error,
        // so the absence has to be representable.
        let plain = EmbedRequest::new("m".into(), vec!["a".into()]);
        assert_eq!(plain.purpose, None);
        assert_eq!(plain.dimensions, None);

        let asked = EmbedRequest::one("m", "a")
            .for_purpose(Purpose::Query)
            .with_dimensions(256);
        assert_eq!(asked.purpose, Some(Purpose::Query));
        assert_eq!(asked.dimensions, Some(256));
        assert_eq!(asked.len(), 1);
    }

    #[test]
    fn an_empty_request_can_be_seen_before_it_reaches_a_network() {
        assert!(EmbedRequest::new("m".into(), vec![]).is_empty());
        assert!(!EmbedRequest::one("m", "a").is_empty());
    }

    #[test]
    fn a_reply_reports_one_dimension_count_only_when_its_vectors_agree() {
        let model: ModelId = "m".into();
        let agreeing = Embeddings::new(
            vec![vector("m", &[1.0, 2.0]), vector("m", &[3.0, 4.0])],
            model.clone(),
            Usage::absent(),
        );
        assert_eq!(agreeing.dimensions(), Some(2));
        assert_eq!(agreeing.len(), 2);

        let ragged = Embeddings::new(
            vec![vector("m", &[1.0, 2.0]), vector("m", &[3.0])],
            model.clone(),
            Usage::absent(),
        );
        assert_eq!(
            ragged.dimensions(),
            None,
            "worth finding out about rather than averaging over"
        );

        let nothing = Embeddings::new(vec![], model, Usage::absent());
        assert_eq!(nothing.dimensions(), None);
        assert!(nothing.is_empty());
    }

    #[test]
    fn capabilities_start_at_nothing_rather_than_at_something_plausible() {
        let none = EmbeddingCapabilities::none(Reach::FirstPartyApi);
        assert_eq!(none.dimensions, 0);
        assert_eq!(none.max_batch, 0);
        assert!(!none.resizable);
        assert!(!none.purposes);

        let filled = EmbeddingCapabilities::none(Reach::FirstPartyApi)
            .with_dimensions(1_536)
            .with_max_input_tokens(8_191)
            .with_max_batch(2_048)
            .resizable();
        assert_eq!(filled.dimensions, 1_536);
        assert!(filled.resizable);
        assert!(!filled.purposes, "still false unless said");
    }

    #[test]
    fn dropping_the_model_is_something_a_reader_can_see_happening() {
        let reply = Embeddings::new(
            vec![vector("m", &[1.0]), vector("m", &[2.0])],
            "m".into(),
            Usage::absent(),
        );
        assert_eq!(reply.into_vectors(), vec![vec![1.0], vec![2.0]]);
    }
}
