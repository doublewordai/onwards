/// Data for the /v1/models endpoint.
/// This endpoint mimics the openai API's models endpoint. Each 'model' is actually a target to
/// forward requests onto.
use serde::{Deserialize, Serialize};

use crate::target::{Target, Targets};

/// Requests to the /v1/{*} endpoints get forwarded onto OpenAI compatible targets.
/// The target is chosen based on the model specified in the request body.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ExtractedModel<'a> {
    #[serde(borrow)]
    pub(crate) model: &'a str,
}

/// The returned models from the /v1/models endpoint.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct Model {
    /// The model identifier, which can be referenced in the API endpoints.
    pub(crate) id: String,
    /// The Unix timestamp (in seconds) when the model was created.
    pub(crate) created: Option<u32>,
    /// The object type, which is always "model".
    pub(crate) object: String,
    /// The organization that owns the model.
    pub(crate) owned_by: String,
}

impl Model {
    /// Models returned by the /v1/models endpoint are each associated with a target.
    pub(crate) fn from_target(id: &str, _target: &Target) -> Self {
        Model {
            id: id.to_owned(),
            created: None,
            object: "model".into(),
            owned_by: "None".into(),
        }
    }
}

/// The response from the /v1/models endpoint, which is a list of models.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct ListModelResponse {
    /// The object type, which is always "list".
    pub object: String,
    /// A list of model objects.
    pub data: Vec<Model>,
}

impl ListModelResponse {
    /// Creates a new ListModelResponse from the given Targets.
    pub(crate) fn from_targets<C>(targets: &Targets<C>) -> Self
    where
        C: governor::clock::Clock + Clone,
    {
        let data = targets
            .targets
            .iter()
            .map(|item| Model::from_target(item.key(), item.value()))
            .collect::<Vec<_>>();
        ListModelResponse {
            object: "list".into(),
            data,
        }
    }
}
