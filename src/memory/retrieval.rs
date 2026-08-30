use crate::error::{JumabekError, JumabekResult};
use crate::memory::facts::Fact;

pub const KEPT: usize = 40;

pub fn to_blob(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|n| n.to_le_bytes()).collect()
}

pub fn from_blob(blob: &[u8]) -> Vec<f32> {
    blob.as_chunks::<4>()
        .0
        .iter()
        .map(|four| f32::from_le_bytes(*four))
        .collect()
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }

    let mut dot = 0.0;
    let mut left = 0.0;
    let mut right = 0.0;

    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        left += x * x;
        right += y * y;
    }

    if left == 0.0 || right == 0.0 {
        return 0.0;
    }

    dot / (left.sqrt() * right.sqrt())
}

pub struct Candidate {
    pub fact: Fact,
    pub vector: Option<Vec<f32>>,
}

pub fn choose(
    candidates: Vec<Candidate>,
    query: &[f32],
    project: Option<&str>,
    limit: usize,
) -> Vec<Fact> {
    let mut pinned = Vec::new();
    let mut ranked: Vec<(f32, Fact)> = Vec::new();

    for candidate in candidates {
        if candidate.fact.pinned {
            pinned.push(candidate.fact);
            continue;
        }

        let similarity = match &candidate.vector {
            Some(vector) => cosine(vector, query),
            None => 0.0,
        };

        let mut weighted = similarity * candidate.fact.scope.weight();

        if let Some(project) = project
            && !candidate.fact.scope_ref.is_empty()
        {
            if candidate.fact.scope_ref == project {
                weighted += 0.15;
            } else {
                weighted -= 0.20;
            }
        }

        ranked.push((weighted, candidate.fact));
    }

    ranked.sort_by(|a, b| b.0.total_cmp(&a.0));

    let room = limit.saturating_sub(pinned.len());
    pinned.extend(ranked.into_iter().take(room).map(|(_, fact)| fact));

    pinned.sort_by(|a, b| {
        (b.subject == "me")
            .cmp(&(a.subject == "me"))
            .then_with(|| a.subject.cmp(&b.subject))
            .then_with(|| a.key.cmp(&b.key))
    });

    pinned
}

#[cfg(feature = "retrieval")]
pub struct Embedder {
    model: std::sync::Mutex<fastembed::TextEmbedding>,
}

#[cfg(feature = "retrieval")]
impl Embedder {
    pub const COMPILED_IN: bool = true;

    pub fn open() -> JumabekResult<Embedder> {
        let options = fastembed::InitOptions::new(fastembed::EmbeddingModel::MultilingualE5Small)
            .with_show_download_progress(true);

        let model = fastembed::TextEmbedding::try_new(options).map_err(|e| {
            JumabekError::ConfigError(format!(
                "cannot start the local embedding model: {}. Set [memory] retrieval = false \
                 to carry on loading every fact instead.",
                e
            ))
        })?;

        Ok(Embedder {
            model: std::sync::Mutex::new(model),
        })
    }

    pub fn embed(&self, texts: Vec<String>) -> JumabekResult<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let mut model = self
            .model
            .lock()
            .map_err(|_| JumabekError::InternalError("the embedding model died".to_string()))?;

        model
            .embed(texts, None)
            .map_err(|e| JumabekError::InternalError(format!("could not embed: {}", e)))
    }
}

#[cfg(not(feature = "retrieval"))]
pub struct Embedder;

#[cfg(not(feature = "retrieval"))]
impl Embedder {
    pub const COMPILED_IN: bool = false;

    pub fn open() -> JumabekResult<Embedder> {
        Err(JumabekError::ConfigError(
            "[memory] retrieval is on, but this build was made without the 'retrieval' feature, \
             so there is no embedding model in it. Rebuild with --features retrieval, or set \
             retrieval = false to carry on loading every fact."
                .to_string(),
        ))
    }

    pub fn embed(&self, _texts: Vec<String>) -> JumabekResult<Vec<Vec<f32>>> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::facts::Scope;

    fn candidate(subject: &str, key: &str, vector: Option<Vec<f32>>) -> Candidate {
        Candidate {
            fact: Fact::new(subject, key, "value"),
            vector,
        }
    }

    fn scoped(subject: &str, scope: Scope, scope_ref: &str, vector: Vec<f32>) -> Candidate {
        Candidate {
            fact: Fact::new(subject, "key", "value").about(scope, scope_ref),
            vector: Some(vector),
        }
    }

    #[test]
    fn a_vector_survives_the_trip_through_the_database() {
        let original = vec![0.5, -0.25, 1.0, 0.0];
        assert_eq!(from_blob(&to_blob(&original)), original);
    }

    #[test]
    fn a_blob_that_is_not_a_whole_number_of_floats_is_read_as_far_as_it_goes() {
        let mut blob = to_blob(&[1.0, 2.0]);
        blob.push(9);
        assert_eq!(from_blob(&blob), vec![1.0, 2.0]);
    }

    #[test]
    fn the_same_direction_scores_higher_than_a_different_one() {
        let query = vec![1.0, 0.0];
        assert!(cosine(&query, &[1.0, 0.0]) > cosine(&query, &[0.0, 1.0]));
        assert!((cosine(&query, &[2.0, 0.0]) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn comparing_against_nothing_scores_nothing_rather_than_dividing_by_zero() {
        assert_eq!(cosine(&[], &[]), 0.0);
        assert_eq!(cosine(&[1.0, 0.0], &[0.0, 0.0]), 0.0);
        assert_eq!(cosine(&[1.0], &[1.0, 0.0]), 0.0);
    }

    #[test]
    fn a_pinned_fact_is_kept_however_little_it_matches() {
        let mut pinned = candidate("me", "name", Some(vec![0.0, 1.0]));
        pinned.fact.pinned = true;

        let chosen = choose(
            vec![pinned, candidate("other", "thing", Some(vec![1.0, 0.0]))],
            &[1.0, 0.0],
            None,
            1,
        );

        assert!(
            chosen.iter().any(|fact| fact.key == "name"),
            "a pinned fact was dropped: {chosen:?}"
        );
    }

    #[test]
    fn pinned_facts_alone_can_fill_the_room_and_nothing_breaks() {
        let mut one = candidate("a", "one", None);
        one.fact.pinned = true;
        let mut two = candidate("b", "two", None);
        two.fact.pinned = true;

        let chosen = choose(
            vec![one, two, candidate("c", "three", None)],
            &[1.0],
            None,
            1,
        );
        assert_eq!(chosen.len(), 2, "a pinned fact was cut to fit: {chosen:?}");
    }

    #[test]
    fn the_closest_facts_are_the_ones_that_survive_the_cut() {
        let chosen = choose(
            vec![
                candidate("far", "k", Some(vec![0.0, 1.0])),
                candidate("near", "k", Some(vec![1.0, 0.0])),
            ],
            &[1.0, 0.0],
            None,
            1,
        );

        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].subject, "near");
    }

    #[test]
    fn a_fact_with_no_vector_yet_is_ranked_last_rather_than_dropped() {
        let chosen = choose(
            vec![
                candidate("unembedded", "k", None),
                candidate("embedded", "k", Some(vec![1.0, 0.0])),
            ],
            &[1.0, 0.0],
            None,
            2,
        );

        assert_eq!(chosen.len(), 2, "a fact awaiting its vector vanished");
    }

    #[test]
    fn a_broadly_true_fact_can_beat_a_narrowly_similar_one() {
        let chosen = choose(
            vec![
                scoped("narrow", Scope::Project, "", vec![0.99, 0.14]),
                scoped("broad", Scope::Global, "", vec![0.97, 0.24]),
            ],
            &[1.0, 0.0],
            None,
            1,
        );

        assert_eq!(
            chosen[0].subject, "broad",
            "scope weighting did nothing: {chosen:?}"
        );
    }

    #[test]
    fn a_fact_about_another_project_is_pushed_down_when_a_project_is_named() {
        let chosen = choose(
            vec![
                scoped("elsewhere", Scope::Project, "shop", vec![1.0, 0.0]),
                scoped("here", Scope::Project, "crm", vec![0.9, 0.44]),
            ],
            &[1.0, 0.0],
            Some("crm"),
            1,
        );

        assert_eq!(
            chosen[0].scope_ref, "crm",
            "another project's detail won: {chosen:?}"
        );
    }

    #[test]
    fn naming_no_project_leaves_every_project_fact_on_equal_footing() {
        let chosen = choose(
            vec![
                scoped("elsewhere", Scope::Project, "shop", vec![1.0, 0.0]),
                scoped("here", Scope::Project, "crm", vec![0.9, 0.44]),
            ],
            &[1.0, 0.0],
            None,
            1,
        );

        assert_eq!(chosen[0].scope_ref, "shop", "{chosen:?}");
    }

    #[test]
    fn what_is_known_about_the_user_still_comes_first_after_ranking() {
        let chosen = choose(
            vec![
                candidate("aaa", "k", Some(vec![1.0, 0.0])),
                candidate("me", "k", Some(vec![0.9, 0.1])),
            ],
            &[1.0, 0.0],
            None,
            2,
        );

        assert_eq!(chosen[0].subject, "me", "{chosen:?}");
    }

    #[test]
    fn asking_for_none_gives_back_only_what_was_pinned() {
        let mut pinned = candidate("a", "one", None);
        pinned.fact.pinned = true;

        let chosen = choose(vec![pinned, candidate("b", "two", None)], &[1.0], None, 0);
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].key, "one");
    }
}
