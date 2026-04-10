use std::collections::BTreeMap;

use ha_types::context::Context;
use ha_types::entity::State;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::state_store::{StateAttributes, StateError, StateStore, make_state_with_context};

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceFieldView {
    pub field: String,
    pub required: bool,
    pub selector: Option<Value>,
}

#[derive(Clone)]
pub struct ServiceDescription {
    pub service: String,
    pub name: String,
    pub description: String,
    pub fields: BTreeMap<String, ServiceField>,
    pub supports_response: SupportsResponse,
}

impl ServiceDescription {
    pub fn fields_view(&self) -> Vec<ServiceFieldView> {
        self.fields
            .iter()
            .map(|(field, descriptor)| ServiceFieldView {
                field: field.clone(),
                required: descriptor.required,
                selector: descriptor.selector.clone(),
            })
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceDescriptorView {
    pub service: String,
    pub name: String,
    pub description: String,
    pub fields: Vec<ServiceFieldView>,
    pub supports_response: SupportsResponse,
}

impl From<ServiceDescription> for ServiceDescriptorView {
    fn from(value: ServiceDescription) -> Self {
        let fields = value.fields_view();
        Self {
            service: value.service,
            name: value.name,
            description: value.description,
            fields,
            supports_response: value.supports_response,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceDomainCatalog {
    pub domain: String,
    pub services: Vec<ServiceDescriptorView>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServiceTarget {
    pub entity_ids: Vec<String>,
    pub device_ids: Vec<String>,
}

impl ServiceTarget {
    pub fn from_parts(
        target: Option<&Value>,
        service_data: Option<&Map<String, Value>>,
    ) -> Result<Self, ServiceError> {
        let mut selection = Self::default();

        if let Some(data) = service_data {
            if let Some(value) = data.get("entity_id") {
                selection.entity_ids = normalize_target_value(value, "entity_id")?;
            }
            if let Some(value) = data.get("device_id") {
                selection.device_ids = normalize_target_value(value, "device_id")?;
            }
        }

        if let Some(Value::Object(target)) = target {
            if let Some(value) = target.get("entity_id") {
                selection.entity_ids = normalize_target_value(value, "entity_id")?;
            }
            if let Some(value) = target.get("device_id") {
                selection.device_ids = normalize_target_value(value, "device_id")?;
            }
        }

        Ok(selection)
    }

    pub fn primary_entity_id(&self) -> Option<&str> {
        self.entity_ids.first().map(String::as_str)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServiceData {
    pub brightness: Option<i64>,
}

impl ServiceData {
    pub fn from_json(data: &Map<String, Value>) -> Result<Self, ServiceError> {
        let brightness = match data.get("brightness") {
            Some(Value::Number(number)) => number.as_i64(),
            Some(Value::Null) | None => None,
            Some(_) => {
                return Err(ServiceError::InvalidFormat(
                    "brightness must be a number".into(),
                ));
            }
        };

        Ok(Self { brightness })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceCall {
    pub domain: String,
    pub service: String,
    pub target: ServiceTarget,
    pub data: ServiceData,
    pub return_response: bool,
}

pub struct ServiceRequest {
    pub domain: String,
    pub service: String,
    pub target: ServiceTarget,
    pub data: ServiceData,
    pub context: Context,
    pub return_response: bool,
}

#[derive(Clone, Debug)]
pub struct ServiceOutcome {
    pub context: Context,
    pub changed_states: Vec<State>,
    pub response: Option<Value>,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ServiceError {
    #[error("Service {domain}.{service} not found.")]
    NotFound { domain: String, service: String },
    #[error("{0}")]
    InvalidFormat(String),
    #[error("{0}")]
    ServiceValidation(String),
    #[error("{0}")]
    HomeAssistant(String),
    #[error("{0}")]
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

    pub fn as_json(&self) -> Value {
        match self {
            Self::NotFound { domain, service } => json!({
                "code": self.code(),
                "message": self.to_string(),
                "translation_key": "service_not_found",
                "translation_domain": "homeassistant",
                "translation_placeholders": {
                    "domain": domain,
                    "service": service
                }
            }),
            _ => json!({
                "code": self.code(),
                "message": self.to_string()
            }),
        }
    }
}

impl From<StateError> for ServiceError {
    fn from(e: StateError) -> Self {
        ServiceError::InvalidFormat(e.to_string())
    }
}

pub struct ServiceRegistry;

impl ServiceRegistry {
    pub fn new() -> Self {
        Self
    }

    pub fn describe(&self) -> Vec<ServiceDomainCatalog> {
        let mut domains: BTreeMap<String, Vec<ServiceDescriptorView>> = BTreeMap::new();
        for definition in BUILTIN_SERVICES {
            domains
                .entry(definition.domain.to_string())
                .or_default()
                .push(definition.description().into());
        }
        domains
            .into_iter()
            .map(|(domain, services)| ServiceDomainCatalog { domain, services })
            .collect()
    }

    pub fn call(
        &self,
        states: &StateStore,
        call: &ServiceCall,
    ) -> Result<ServiceOutcome, ServiceError> {
        let definition = find_builtin_service(&call.domain, &call.service)
            .ok_or_else(|| ServiceError::NotFound {
                domain: call.domain.clone(),
                service: call.service.clone(),
            })?;

        match (
            definition.description().supports_response,
            call.return_response,
        ) {
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

        if let Some(validator) = definition.validator() {
            validator(call)?;
        }

        let context = new_context();
        definition.invoke(
            ServiceRequest {
                domain: call.domain.clone(),
                service: call.service.clone(),
                target: call.target.clone(),
                data: call.data.clone(),
                context: context.clone(),
                return_response: call.return_response,
            },
            states,
        )
    }
}

impl Default for ServiceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn require_entity_id(call: &ServiceCall) -> Result<(), ServiceError> {
    call.target
        .primary_entity_id()
        .map(|_| ())
        .ok_or_else(|| ServiceError::InvalidFormat("target must include entity_id".into()))
}

fn set_entities_state(
    request: ServiceRequest,
    states: &StateStore,
    state_value: &str,
) -> Result<ServiceOutcome, ServiceError> {
    let entity_ids = if request.target.entity_ids.is_empty() {
        None
    } else {
        Some(request.target.entity_ids.clone())
    }
        .ok_or_else(|| ServiceError::InvalidFormat("target must include entity_id".into()))?;
    let mut changed_states = Vec::new();
    for entity_id in entity_ids {
        let mut attributes = states
            .get(&entity_id)
            .map(|state| state.attributes)
            .unwrap_or_default();
        if let Some(brightness) = request.data.brightness {
            attributes.insert("brightness".into(), json!(brightness));
        }
        let new_state = make_state_with_context(
            entity_id.clone(),
            state_value.to_string(),
            StateAttributes::from_hash(attributes),
            request.context.clone(),
        );
        states.set(new_state.clone())?;
        changed_states.push(new_state);
    }
    Ok(ServiceOutcome {
        context: request.context,
        changed_states,
        response: None,
    })
}

fn normalize_target_value(value: &Value, key: &str) -> Result<Vec<String>, ServiceError> {
    match value {
        Value::String(value) => {
            if value.contains("{{") {
                return Err(ServiceError::InvalidFormat(format!(
                    "templates are not allowed in target {key}"
                )));
            }
            Ok(vec![value.clone()])
        }
        Value::Array(values) => values
            .iter()
            .map(|item| {
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
                Ok(value.to_string())
            })
            .collect(),
        _ => Err(ServiceError::InvalidFormat(format!(
            "target {key} must be a string or list"
        ))),
    }
}

#[derive(Clone, Copy)]
struct BuiltinServiceDefinition {
    domain: &'static str,
    service: &'static str,
    kind: BuiltinServiceKind,
}

impl BuiltinServiceDefinition {
    fn description(&self) -> ServiceDescription {
        match self.kind {
            BuiltinServiceKind::LightTurnOn => ServiceDescription {
                service: self.service.into(),
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
            BuiltinServiceKind::LightTurnOff => ServiceDescription {
                service: self.service.into(),
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
            BuiltinServiceKind::SwitchTurnOn => ServiceDescription {
                service: self.service.into(),
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
            BuiltinServiceKind::SwitchTurnOff => ServiceDescription {
                service: self.service.into(),
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
        }
    }

    fn validator(&self) -> Option<fn(&ServiceCall) -> Result<(), ServiceError>> {
        Some(require_entity_id)
    }

    fn invoke(
        &self,
        request: ServiceRequest,
        states: &StateStore,
    ) -> Result<ServiceOutcome, ServiceError> {
        match self.kind {
            BuiltinServiceKind::LightTurnOn | BuiltinServiceKind::SwitchTurnOn => {
                set_entities_state(request, states, "on")
            }
            BuiltinServiceKind::LightTurnOff | BuiltinServiceKind::SwitchTurnOff => {
                set_entities_state(request, states, "off")
            }
        }
    }
}

#[derive(Clone, Copy)]
enum BuiltinServiceKind {
    LightTurnOn,
    LightTurnOff,
    SwitchTurnOn,
    SwitchTurnOff,
}

const BUILTIN_SERVICES: &[BuiltinServiceDefinition] = &[
    BuiltinServiceDefinition {
        domain: "light",
        service: "turn_on",
        kind: BuiltinServiceKind::LightTurnOn,
    },
    BuiltinServiceDefinition {
        domain: "light",
        service: "turn_off",
        kind: BuiltinServiceKind::LightTurnOff,
    },
    BuiltinServiceDefinition {
        domain: "switch",
        service: "turn_on",
        kind: BuiltinServiceKind::SwitchTurnOn,
    },
    BuiltinServiceDefinition {
        domain: "switch",
        service: "turn_off",
        kind: BuiltinServiceKind::SwitchTurnOff,
    },
];

fn find_builtin_service(domain: &str, service: &str) -> Option<BuiltinServiceDefinition> {
    BUILTIN_SERVICES
        .iter()
        .copied()
        .find(|definition| definition.domain == domain && definition.service == service)
}

fn new_context() -> Context {
    Context {
        id: Uuid::new_v4().to_string().replace('-', ""),
        parent_id: None,
        user_id: None,
    }
}
