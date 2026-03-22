use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};

use ha_types::context::Context;
use ha_types::entity::State;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::app::AppState;
use crate::state_store::make_state_with_context;

type ServiceHandler =
    Arc<dyn Fn(ServiceRequest, &AppState) -> Result<ServiceOutcome, ServiceError> + Send + Sync>;
type ServiceValidator = Arc<dyn Fn(&Map<String, Value>) -> Result<(), ServiceError> + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportsResponse {
    None,
    Optional,
    Only,
}

#[derive(Clone)]
pub struct ServiceField {
    pub required: bool,
    pub selector: Option<Value>,
}

#[derive(Clone)]
pub struct ServiceDescription {
    pub name: String,
    pub description: String,
    pub fields: BTreeMap<String, ServiceField>,
    pub supports_response: SupportsResponse,
}

impl ServiceDescription {
    pub fn as_json(&self) -> Value {
        let fields = self
            .fields
            .iter()
            .map(|(key, field)| {
                (
                    key.clone(),
                    json!({
                        "required": field.required,
                        "selector": field.selector
                    }),
                )
            })
            .collect::<Map<String, Value>>();
        json!({
            "name": self.name,
            "description": self.description,
            "fields": fields,
        })
    }
}

pub struct ServiceRequest {
    pub domain: String,
    pub service: String,
    pub data: Map<String, Value>,
    pub context: Context,
    pub return_response: bool,
}

pub struct ServiceOutcome {
    pub context: Context,
    pub changed_states: Vec<State>,
    pub response: Option<Value>,
}

#[derive(Debug, Clone)]
pub enum ServiceError {
    NotFound { domain: String, service: String },
    InvalidFormat(String),
    ServiceValidation(String),
    HomeAssistant(String),
    Unknown(String),
}

impl ServiceError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound { .. } => "not_found",
            Self::InvalidFormat(_) => "invalid_format",
            Self::ServiceValidation(_) => "service_validation_error",
            Self::HomeAssistant(_) => "home_assistant_error",
            Self::Unknown(_) => "unknown_error",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::NotFound { domain, service } => {
                format!("Service {domain}.{service} not found.")
            }
            Self::InvalidFormat(message)
            | Self::ServiceValidation(message)
            | Self::HomeAssistant(message)
            | Self::Unknown(message) => message.clone(),
        }
    }

    pub fn as_json(&self) -> Value {
        match self {
            Self::NotFound { domain, service } => json!({
                "code": self.code(),
                "message": self.message(),
                "translation_key": "service_not_found",
                "translation_domain": "homeassistant",
                "translation_placeholders": {
                    "domain": domain,
                    "service": service
                }
            }),
            _ => json!({
                "code": self.code(),
                "message": self.message()
            }),
        }
    }
}

struct ServiceEntry {
    description: ServiceDescription,
    validator: Option<ServiceValidator>,
    handler: ServiceHandler,
}

pub struct ServiceRegistry {
    entries: RwLock<HashMap<(String, String), ServiceEntry>>,
}

impl ServiceRegistry {
    pub fn new() -> Self {
        let registry = Self {
            entries: RwLock::new(HashMap::new()),
        };
        registry.register_builtin_services();
        registry
    }

    pub fn register<F, V>(
        &self,
        domain: &str,
        service: &str,
        description: ServiceDescription,
        validator: Option<V>,
        handler: F,
    ) where
        F: Fn(ServiceRequest, &AppState) -> Result<ServiceOutcome, ServiceError>
            + Send
            + Sync
            + 'static,
        V: Fn(&Map<String, Value>) -> Result<(), ServiceError> + Send + Sync + 'static,
    {
        let entry = ServiceEntry {
            description,
            validator: validator.map(|validator| Arc::new(validator) as ServiceValidator),
            handler: Arc::new(handler),
        };
        self.entries
            .write()
            .expect("service registry lock poisoned")
            .insert((domain.to_string(), service.to_string()), entry);
    }

    pub fn describe(&self) -> Value {
        let entries = self.entries.read().expect("service registry lock poisoned");
        let mut domains: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
        for ((domain, service), entry) in &*entries {
            domains
                .entry(domain.clone())
                .or_default()
                .insert(service.clone(), entry.description.as_json());
        }
        serde_json::to_value(domains).unwrap_or_else(|_| json!({}))
    }

    pub fn call(
        &self,
        app: &AppState,
        domain: &str,
        service: &str,
        mut data: Map<String, Value>,
        target: Option<&Value>,
        return_response: bool,
    ) -> Result<ServiceOutcome, ServiceError> {
        merge_target(&mut data, target)?;

        let entries = self.entries.read().expect("service registry lock poisoned");
        let entry = entries
            .get(&(domain.to_string(), service.to_string()))
            .ok_or_else(|| ServiceError::NotFound {
                domain: domain.to_string(),
                service: service.to_string(),
            })?;

        match (entry.description.supports_response, return_response) {
            (SupportsResponse::None, true) => {
                return Err(ServiceError::ServiceValidation(
                    "Service does not support responses. Remove return_response from request."
                        .into(),
                ));
            }
            (SupportsResponse::Only, false) => {
                return Err(ServiceError::ServiceValidation(
                    "Service call requires responses but caller did not ask for responses. Add return_response to request.".into(),
                ));
            }
            _ => {}
        }

        if let Some(validator) = &entry.validator {
            validator(&data)?;
        }

        let context = new_context();
        (entry.handler)(
            ServiceRequest {
                domain: domain.to_string(),
                service: service.to_string(),
                data,
                context: context.clone(),
                return_response,
            },
            app,
        )
    }

    fn register_builtin_services(&self) {
        self.register(
            "light",
            "turn_on",
            ServiceDescription {
                name: "Turn on".into(),
                description: "Turn on light entities.".into(),
                fields: BTreeMap::from([
                    (
                        "entity_id".into(),
                        ServiceField {
                            required: false,
                            selector: Some(json!({"entity": {"domain": "light"}})),
                        },
                    ),
                    (
                        "brightness".into(),
                        ServiceField {
                            required: false,
                            selector: Some(json!({"number": {"min": 0, "max": 255}})),
                        },
                    ),
                ]),
                supports_response: SupportsResponse::None,
            },
            Some(require_entity_id),
            |request, app| set_entities_state(request, app, "on"),
        );
        self.register(
            "light",
            "turn_off",
            ServiceDescription {
                name: "Turn off".into(),
                description: "Turn off light entities.".into(),
                fields: BTreeMap::from([(
                    "entity_id".into(),
                    ServiceField {
                        required: false,
                        selector: Some(json!({"entity": {"domain": "light"}})),
                    },
                )]),
                supports_response: SupportsResponse::None,
            },
            Some(require_entity_id),
            |request, app| set_entities_state(request, app, "off"),
        );
        self.register(
            "switch",
            "turn_on",
            ServiceDescription {
                name: "Turn on".into(),
                description: "Turn on switch entities.".into(),
                fields: BTreeMap::from([(
                    "entity_id".into(),
                    ServiceField {
                        required: false,
                        selector: Some(json!({"entity": {"domain": "switch"}})),
                    },
                )]),
                supports_response: SupportsResponse::None,
            },
            Some(require_entity_id),
            |request, app| set_entities_state(request, app, "on"),
        );
        self.register(
            "switch",
            "turn_off",
            ServiceDescription {
                name: "Turn off".into(),
                description: "Turn off switch entities.".into(),
                fields: BTreeMap::from([(
                    "entity_id".into(),
                    ServiceField {
                        required: false,
                        selector: Some(json!({"entity": {"domain": "switch"}})),
                    },
                )]),
                supports_response: SupportsResponse::None,
            },
            Some(require_entity_id),
            |request, app| set_entities_state(request, app, "off"),
        );
    }
}

impl Default for ServiceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn require_entity_id(data: &Map<String, Value>) -> Result<(), ServiceError> {
    entity_ids_from_data(data)
        .map(|_| ())
        .ok_or_else(|| ServiceError::InvalidFormat("target must include entity_id".into()))
}

fn set_entities_state(
    request: ServiceRequest,
    app: &AppState,
    state_value: &str,
) -> Result<ServiceOutcome, ServiceError> {
    let entity_ids = entity_ids_from_data(&request.data)
        .ok_or_else(|| ServiceError::InvalidFormat("target must include entity_id".into()))?;
    let mut changed_states = Vec::new();
    for entity_id in entity_ids {
        let mut attributes = app
            .states
            .get(&entity_id)
            .map(|state| state.attributes)
            .unwrap_or_default();
        if let Some(brightness) = request.data.get("brightness") {
            attributes.insert("brightness".into(), brightness.clone());
        }
        let new_state = make_state_with_context(
            entity_id.clone(),
            state_value.to_string(),
            attributes,
            request.context.clone(),
        );
        app.states
            .set(new_state.clone())
            .map_err(ServiceError::InvalidFormat)?;
        changed_states.push(new_state);
    }
    Ok(ServiceOutcome {
        context: request.context,
        changed_states,
        response: None,
    })
}

fn entity_ids_from_data(data: &Map<String, Value>) -> Option<Vec<String>> {
    let entity_value = data.get("entity_id")?;
    match entity_value {
        Value::String(entity_id) => Some(vec![entity_id.clone()]),
        Value::Array(values) => values
            .iter()
            .map(|value| value.as_str().map(ToOwned::to_owned))
            .collect::<Option<Vec<_>>>(),
        _ => None,
    }
}

fn merge_target(data: &mut Map<String, Value>, target: Option<&Value>) -> Result<(), ServiceError> {
    let Some(Value::Object(target)) = target else {
        return Ok(());
    };

    for key in ["entity_id", "device_id"] {
        let Some(value) = target.get(key) else {
            continue;
        };

        let normalized = match value {
            Value::String(value) => {
                if value.contains("{{") {
                    return Err(ServiceError::InvalidFormat(format!(
                        "templates are not allowed in target {key}"
                    )));
                }
                Value::Array(vec![Value::String(value.clone())])
            }
            Value::Array(values) => {
                for item in values {
                    let Some(value) = item.as_str() else {
                        return Err(ServiceError::InvalidFormat(format!(
                            "target {key} entries must be strings"
                        )));
                    };
                    if value.contains("{{") {
                        return Err(ServiceError::InvalidFormat(format!(
                            "templates are not allowed in target {key}"
                        )));
                    }
                }
                Value::Array(values.clone())
            }
            _ => {
                return Err(ServiceError::InvalidFormat(format!(
                    "target {key} must be a string or list"
                )));
            }
        };
        data.insert(key.to_string(), normalized);
    }

    Ok(())
}

fn new_context() -> Context {
    Context {
        id: Uuid::new_v4().to_string().replace('-', ""),
        parent_id: None,
        user_id: None,
    }
}
