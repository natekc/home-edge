use std::ops::{BitOr, BitOrAssign};
use std::sync::atomic::{AtomicU8, Ordering};

#[cfg(feature = "transport_wifi")]
use anyhow::Result;
use ha_types::api::{ApiConfigResponse, ApiStatusResponse, UnitSystem};
use ha_types::core_state::{CoreState, CoreStateResponse, RecorderState};
use ha_types::entity::State;
use serde_json::{Map, Value};

#[cfg(feature = "transport_wifi")]
use crate::auth_store::{AuthStore, AuthUser};
use crate::config::AppConfig;
use crate::service::{
    ServiceCall, ServiceDomainCatalog, ServiceError, ServiceOutcome, ServiceRegistry,
};
use crate::state_store::{StateStore, make_state};
#[cfg(feature = "transport_wifi")]
use crate::storage::{OnboardingState as PersistedOnboardingState, Storage, StoredUser};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum BuildTransport {
    Wifi = 1,
    Ble = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RuntimeMode {
    UnprovisionedWifi = 1,
    WifiOperational = 2,
    UnprovisionedBle = 3,
    BleOperational = 4,
    Maintenance = 5,
    Disabled = 6,
}

impl RuntimeMode {
    pub const fn default_for_build() -> Self {
        #[cfg(feature = "transport_wifi")]
        {
            Self::UnprovisionedWifi
        }

        #[cfg(feature = "transport_ble")]
        {
            Self::UnprovisionedBle
        }
    }

    pub const fn operational_for_build() -> Self {
        #[cfg(feature = "transport_wifi")]
        {
            Self::WifiOperational
        }

        #[cfg(feature = "transport_ble")]
        {
            Self::BleOperational
        }
    }

    pub const fn from_persisted_onboarding(onboarded: bool) -> Self {
        if onboarded {
            Self::operational_for_build()
        } else {
            Self::default_for_build()
        }
    }

    pub const fn build_transport(self) -> BuildTransport {
        match self {
            Self::UnprovisionedWifi | Self::WifiOperational => BuildTransport::Wifi,
            Self::UnprovisionedBle | Self::BleOperational => BuildTransport::Ble,
            Self::Maintenance | Self::Disabled => current_build_transport(),
        }
    }

    const fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::UnprovisionedWifi,
            2 => Self::WifiOperational,
            3 => Self::UnprovisionedBle,
            4 => Self::BleOperational,
            5 => Self::Maintenance,
            6 => Self::Disabled,
            _ => Self::default_for_build(),
        }
    }
}

pub const fn current_build_transport() -> BuildTransport {
    #[cfg(feature = "transport_wifi")]
    {
        BuildTransport::Wifi
    }

    #[cfg(feature = "transport_ble")]
    {
        BuildTransport::Ble
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ModeTransition {
    CompleteOnboarding = 1,
    EnterMaintenance = 2,
    ExitMaintenance = 3,
    EnableOperationalMode = 4,
    DisableInteractiveTransport = 5,
    FactoryReset = 6,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum OwnershipState {
    Unclaimed = 1,
    Claimed = 2,
    TransferPending = 3,
    ResetPending = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PowerStateSummary {
    Awake = 1,
    SleepEligible = 2,
    Sleeping = 3,
    Waking = 4,
    Sampling = 5,
    Degraded = 6,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Discoverability {
    Hidden = 1,
    OnboardingOnly = 2,
    Operational = 3,
    MaintenanceOnly = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Connectability {
    Disabled = 1,
    ClaimOnly = 2,
    AuthenticatedOnly = 3,
    Mixed = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum AuthPolicy {
    None = 1,
    OnboardingClaim = 2,
    TokenSession = 3,
    BondedSession = 4,
    MaintenanceOnly = 5,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PowerPolicy {
    AlwaysOn = 1,
    SleepyCachedReads = 2,
    WakeForLiveReads = 3,
    WakeForCommands = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum EventPolicy {
    NoEvents = 1,
    MinimalNotifications = 2,
    RichStreamUnsupported = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CompatibilityPolicy {
    NativeOnly = 1,
    WifiHaCompat = 2,
    BleCompactProtocol = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum TransitionPolicy {
    Disallowed = 1,
    MaintenanceOnly = 2,
    Allowed = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PermissionFlags(u32);

impl PermissionFlags {
    pub const NONE: Self = Self(0);
    pub const READ_STATE: Self = Self(1 << 0);
    pub const WRITE_STATE: Self = Self(1 << 1);
    pub const CALL_SERVICE: Self = Self(1 << 2);
    pub const CONFIGURE: Self = Self(1 << 3);
    pub const MANAGE_MODE: Self = Self(1 << 4);

    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
}

impl Default for PermissionFlags {
    fn default() -> Self {
        Self::NONE
    }
}

impl BitOr for PermissionFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for PermissionFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PrincipalKind {
    AnonymousClaim = 1,
    OwnerApp = 2,
    MaintenanceClient = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum AuthStrength {
    Unauthenticated = 1,
    ClaimVerified = 2,
    TokenVerified = 3,
    Bonded = 4,
    MaintenanceVerified = 5,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DeviceBindingState {
    Unbound = 1,
    Bound = 2,
    RotationPending = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum FreshnessRequirement {
    CachedAllowed = 1,
    LivePreferred = 2,
    LiveRequired = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionContext {
    pub principal: PrincipalKind,
    pub auth_strength: AuthStrength,
    pub permissions: PermissionFlags,
    pub session_id: u64,
    pub binding: DeviceBindingState,
    pub freshness: FreshnessRequirement,
    pub maintenance_scope: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Consistency {
    CachedAllowed = 1,
    LivePreferred = 2,
    LiveRequired = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DeadlineClass {
    Background = 1,
    Interactive = 2,
    Immediate = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OperationMeta {
    pub request_id: u32,
    pub consistency: Consistency,
    pub deadline: DeadlineClass,
    pub allow_cached: bool,
    pub allow_deferred: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum OperationError {
    InvalidRequest = 1,
    Unauthorized = 2,
    Forbidden = 3,
    NotFound = 4,
    UnsupportedInMode = 5,
    WakeRequired = 6,
    Busy = 7,
    Conflict = 8,
    StaleCursor = 9,
    DeferredOnly = 10,
    Internal = 11,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum FreshnessState {
    Live = 1,
    CachedFresh = 2,
    CachedStale = 3,
    UnavailableUntilWake = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FreshnessInfo {
    pub state: FreshnessState,
    pub age_ms: u32,
    pub sampled_at_ms: u64,
    pub wake_required: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryCursor {
    pub generation: u32,
    pub offset: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageRequest {
    pub limit: u16,
    pub cursor: Option<QueryCursor>,
    pub include_attributes: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DomainKind {
    Any = 0,
    Sensor = 1,
    BinarySensor = 2,
    Light = 3,
    Switch = 4,
    Other = 255,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StateFilter {
    pub domain: DomainKind,
    pub changed_since: Option<QueryCursor>,
    pub include_attributes: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntityHandle(pub u32);

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum StateValue<'a> {
    Bool(bool),
    Int(i64),
    Float(f64),
    SmallText(&'a str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StateAttr<'a> {
    pub key: &'a str,
    pub value: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EntityStateView<'a> {
    pub entity: EntityHandle,
    pub entity_id: &'a str,
    pub state: StateValue<'a>,
    pub freshness: FreshnessInfo,
    pub context_id: u64,
    pub last_updated_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServiceDescriptor {
    pub domain: DomainKind,
    pub service_id: u16,
    pub capability_flags: u16,
    pub response_required: bool,
    pub schema_id: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeStatus {
    pub mode: RuntimeMode,
    pub ownership: OwnershipState,
    pub power: PowerStateSummary,
    pub freshness: FreshnessInfo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OnboardingStatus {
    pub claimed: bool,
    pub mode: RuntimeMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfigSummary<'a> {
    pub product_name: &'a str,
    pub mode: RuntimeMode,
}

#[cfg(feature = "transport_wifi")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OnboardingProgress {
    pub user_done: bool,
    pub core_config_done: bool,
    pub onboarded: bool,
}

#[cfg(feature = "transport_wifi")]
impl OnboardingProgress {
    fn from_state(state: &PersistedOnboardingState) -> Self {
        Self {
            user_done: state.step_done("user"),
            core_config_done: state.step_done("core_config"),
            onboarded: state.onboarded,
        }
    }
}

#[cfg(feature = "transport_wifi")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OnboardingUserInput {
    pub name: String,
    pub username: String,
    pub password: String,
    pub language: String,
}

#[cfg(feature = "transport_wifi")]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OnboardingCoreConfigInput {
    pub location_name: Option<String>,
    pub country: Option<String>,
    pub language: Option<String>,
    pub time_zone: Option<String>,
    pub unit_system: Option<String>,
}

#[cfg(feature = "transport_wifi")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizeBootstrapInput {
    pub display_name: String,
    pub username: String,
    pub password: String,
    pub location_name: String,
    pub language: String,
}

#[cfg(feature = "transport_wifi")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateOnboardingUserOutcome {
    Created,
    UserStepAlreadyDone,
}

#[cfg(feature = "transport_wifi")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompleteCoreConfigOutcome {
    Completed,
    CoreConfigStepAlreadyDone,
    UserStepRequired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum WakeReason {
    ReadLiveState = 1,
    ExecuteCommand = 2,
    Maintenance = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WakeRequest {
    pub reason: WakeReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WakeStatus {
    pub accepted: bool,
    pub already_awake: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DomainEvent {
    StateChanged = 1,
    ServiceCompleted = 2,
    OnboardingAdvanced = 3,
    OwnershipChanged = 4,
    RuntimeModeChanged = 5,
    PowerStateChanged = 6,
    FaultRaised = 7,
}

#[derive(Clone, Debug)]
pub enum OperationRequest<'a> {
    GetApiStatus,
    GetCoreState,
    GetRuntimeStatus,
    GetOnboardingStatus,
    OpenSession,
    CloseSession,
    GetEntityState { entity_id: &'a str, meta: OperationMeta },
    ListEntityStates { page: PageRequest, filter: StateFilter, meta: OperationMeta },
    SetEntityState {
        entity_id: &'a str,
        state: &'a str,
        attributes: Map<String, Value>,
        meta: OperationMeta,
    },
    ListServices { page: PageRequest, meta: OperationMeta },
    CallService { call: ServiceCall, meta: OperationMeta },
    GetConfigSummary,
    RequestWake { request: WakeRequest },
    RequestTransition { target: ModeTransition },
}

#[derive(Clone, Debug)]
pub enum OperationResult {
    Ack,
    ApiStatus(ApiStatusResponse),
    CoreState(CoreStateResponse),
    RuntimeStatus(RuntimeStatus),
    OnboardingStatus(OnboardingStatus),
    EntityState(State),
    EntityStates(Vec<State>),
    ServiceCatalog(Vec<ServiceDomainCatalog>),
    ServiceCallAccepted,
    ServiceCallCompleted(ServiceOutcome),
    ConfigSummary(ApiConfigResponse),
    WakeStatus(WakeStatus),
    Error(OperationError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransportPolicy {
    pub discoverability: Discoverability,
    pub connectability: Connectability,
    pub auth_policy: AuthPolicy,
    pub power_policy: PowerPolicy,
    pub read_policy: ReadPolicy,
    pub write_policy: WritePolicy,
    pub event_policy: EventPolicy,
    pub compatibility_policy: CompatibilityPolicy,
    pub transition_policy: TransitionPolicy,
    pub max_page_size: u16,
    pub max_event_batch: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadPolicy {
    pub live_allowed: bool,
    pub cached_allowed: bool,
    pub stale_allowed: bool,
    pub paging_required: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WritePolicy {
    pub writes_allowed: bool,
    pub writes_require_auth: bool,
    pub writes_require_wake: bool,
}

#[derive(Debug)]
pub struct ModeController {
    mode: AtomicU8,
}

impl ModeController {
    pub fn new(initial_mode: RuntimeMode) -> Self {
        Self {
            mode: AtomicU8::new(initial_mode as u8),
        }
    }

    pub fn current_mode(&self) -> RuntimeMode {
        RuntimeMode::from_u8(self.mode.load(Ordering::Relaxed))
    }

    pub fn set_mode(&self, mode: RuntimeMode) {
        self.mode.store(mode as u8, Ordering::Relaxed);
    }
}

#[derive(Debug, Default)]
pub struct PolicyResolver;

impl PolicyResolver {
    pub const fn new() -> Self {
        Self
    }

    pub fn policy_for(&self, mode: RuntimeMode) -> TransportPolicy {
        match mode {
            RuntimeMode::UnprovisionedWifi => TransportPolicy {
                discoverability: Discoverability::Operational,
                connectability: Connectability::ClaimOnly,
                auth_policy: AuthPolicy::OnboardingClaim,
                power_policy: PowerPolicy::AlwaysOn,
                read_policy: ReadPolicy {
                    live_allowed: true,
                    cached_allowed: true,
                    stale_allowed: false,
                    paging_required: false,
                },
                write_policy: WritePolicy {
                    writes_allowed: true,
                    writes_require_auth: false,
                    writes_require_wake: false,
                },
                event_policy: EventPolicy::RichStreamUnsupported,
                compatibility_policy: CompatibilityPolicy::WifiHaCompat,
                transition_policy: TransitionPolicy::Allowed,
                max_page_size: 512,
                max_event_batch: 128,
            },
            RuntimeMode::WifiOperational => TransportPolicy {
                discoverability: Discoverability::Operational,
                connectability: Connectability::AuthenticatedOnly,
                auth_policy: AuthPolicy::TokenSession,
                power_policy: PowerPolicy::AlwaysOn,
                read_policy: ReadPolicy {
                    live_allowed: true,
                    cached_allowed: true,
                    stale_allowed: true,
                    paging_required: false,
                },
                write_policy: WritePolicy {
                    writes_allowed: true,
                    writes_require_auth: true,
                    writes_require_wake: false,
                },
                event_policy: EventPolicy::RichStreamUnsupported,
                compatibility_policy: CompatibilityPolicy::WifiHaCompat,
                transition_policy: TransitionPolicy::MaintenanceOnly,
                max_page_size: 512,
                max_event_batch: 256,
            },
            RuntimeMode::UnprovisionedBle => TransportPolicy {
                discoverability: Discoverability::OnboardingOnly,
                connectability: Connectability::ClaimOnly,
                auth_policy: AuthPolicy::OnboardingClaim,
                power_policy: PowerPolicy::SleepyCachedReads,
                read_policy: ReadPolicy {
                    live_allowed: false,
                    cached_allowed: true,
                    stale_allowed: true,
                    paging_required: true,
                },
                write_policy: WritePolicy {
                    writes_allowed: false,
                    writes_require_auth: false,
                    writes_require_wake: true,
                },
                event_policy: EventPolicy::MinimalNotifications,
                compatibility_policy: CompatibilityPolicy::BleCompactProtocol,
                transition_policy: TransitionPolicy::Allowed,
                max_page_size: 32,
                max_event_batch: 16,
            },
            RuntimeMode::BleOperational => TransportPolicy {
                discoverability: Discoverability::Operational,
                connectability: Connectability::AuthenticatedOnly,
                auth_policy: AuthPolicy::BondedSession,
                power_policy: PowerPolicy::WakeForCommands,
                read_policy: ReadPolicy {
                    live_allowed: false,
                    cached_allowed: true,
                    stale_allowed: true,
                    paging_required: true,
                },
                write_policy: WritePolicy {
                    writes_allowed: true,
                    writes_require_auth: true,
                    writes_require_wake: true,
                },
                event_policy: EventPolicy::MinimalNotifications,
                compatibility_policy: CompatibilityPolicy::BleCompactProtocol,
                transition_policy: TransitionPolicy::MaintenanceOnly,
                max_page_size: 32,
                max_event_batch: 16,
            },
            RuntimeMode::Maintenance => TransportPolicy {
                discoverability: Discoverability::MaintenanceOnly,
                connectability: Connectability::AuthenticatedOnly,
                auth_policy: AuthPolicy::MaintenanceOnly,
                power_policy: PowerPolicy::AlwaysOn,
                read_policy: ReadPolicy {
                    live_allowed: true,
                    cached_allowed: true,
                    stale_allowed: true,
                    paging_required: false,
                },
                write_policy: WritePolicy {
                    writes_allowed: true,
                    writes_require_auth: true,
                    writes_require_wake: false,
                },
                event_policy: EventPolicy::NoEvents,
                compatibility_policy: CompatibilityPolicy::NativeOnly,
                transition_policy: TransitionPolicy::Allowed,
                max_page_size: 128,
                max_event_batch: 32,
            },
            RuntimeMode::Disabled => TransportPolicy {
                discoverability: Discoverability::Hidden,
                connectability: Connectability::Disabled,
                auth_policy: AuthPolicy::None,
                power_policy: PowerPolicy::SleepyCachedReads,
                read_policy: ReadPolicy {
                    live_allowed: false,
                    cached_allowed: false,
                    stale_allowed: false,
                    paging_required: false,
                },
                write_policy: WritePolicy {
                    writes_allowed: false,
                    writes_require_auth: false,
                    writes_require_wake: false,
                },
                event_policy: EventPolicy::NoEvents,
                compatibility_policy: CompatibilityPolicy::NativeOnly,
                transition_policy: TransitionPolicy::Disallowed,
                max_page_size: 0,
                max_event_batch: 0,
            },
        }
    }
}

#[derive(Debug)]
pub struct AppCore {
    mode_controller: ModeController,
    policy_resolver: PolicyResolver,
}

pub struct CoreDeps<'a> {
    pub config: &'a AppConfig,
    pub states: &'a StateStore,
    pub services: &'a ServiceRegistry,
}

impl AppCore {
    pub fn new() -> Self {
        Self {
            mode_controller: ModeController::new(RuntimeMode::default_for_build()),
            policy_resolver: PolicyResolver::new(),
        }
    }

    pub fn runtime_mode(&self) -> RuntimeMode {
        self.mode_controller.current_mode()
    }

    pub fn set_runtime_mode(&self, mode: RuntimeMode) {
        self.mode_controller.set_mode(mode);
    }

    pub fn transport_policy(&self) -> TransportPolicy {
        self.policy_resolver.policy_for(self.runtime_mode())
    }

    pub fn execute(&self, deps: CoreDeps<'_>, request: OperationRequest<'_>) -> OperationResult {
        match request {
            OperationRequest::GetApiStatus => OperationResult::ApiStatus(ApiStatusResponse::default()),
            OperationRequest::GetCoreState => OperationResult::CoreState(CoreStateResponse {
                state: CoreState::Running,
                recorder_state: RecorderState {
                    migration_in_progress: false,
                    migration_is_live: false,
                },
            }),
            OperationRequest::GetConfigSummary => OperationResult::ConfigSummary(ApiConfigResponse {
                version: env!("CARGO_PKG_VERSION").into(),
                location_name: deps.config.ui.product_name.clone(),
                time_zone: "UTC".into(),
                language: "en".into(),
                latitude: 0.0,
                longitude: 0.0,
                elevation: 0.0,
                unit_system: UnitSystem::metric(),
                state: "RUNNING".into(),
                components: vec!["api".into(), "core".into()],
                whitelist_external_dirs: vec![],
            }),
            OperationRequest::GetEntityState { entity_id, .. } => match deps.states.get(entity_id) {
                Some(state) => OperationResult::EntityState(state),
                None => OperationResult::Error(OperationError::NotFound),
            },
            OperationRequest::ListEntityStates { .. } => {
                OperationResult::EntityStates(deps.states.all())
            }
            OperationRequest::SetEntityState {
                entity_id,
                state,
                attributes,
                ..
            } => {
                if !State::is_valid_entity_id(entity_id) {
                    return OperationResult::Error(OperationError::InvalidRequest);
                }
                if state.len() > 255 {
                    return OperationResult::Error(OperationError::InvalidRequest);
                }

                let attrs = attributes.into_iter().collect();
                let next_state = make_state(entity_id, state, attrs);
                if deps.states.set(next_state).is_err() {
                    return OperationResult::Error(OperationError::InvalidRequest);
                }

                match deps.states.get(entity_id) {
                    Some(saved) => OperationResult::EntityState(saved),
                    None => OperationResult::Error(OperationError::Internal),
                }
            }
            OperationRequest::ListServices { .. } => {
                OperationResult::ServiceCatalog(deps.services.describe())
            }
            OperationRequest::CallService { call, .. } => {
                match deps.services.call(deps.states, &call) {
                    Ok(outcome) => OperationResult::ServiceCallCompleted(outcome),
                    Err(ServiceError::NotFound { .. }) => OperationResult::Error(OperationError::NotFound),
                    Err(ServiceError::InvalidFormat(_))
                    | Err(ServiceError::ServiceValidation(_)) => {
                        OperationResult::Error(OperationError::InvalidRequest)
                    }
                    Err(_) => OperationResult::Error(OperationError::Internal),
                }
            }
            OperationRequest::GetRuntimeStatus => OperationResult::RuntimeStatus(RuntimeStatus {
                mode: self.runtime_mode(),
                ownership: OwnershipState::Unclaimed,
                power: PowerStateSummary::Awake,
                freshness: FreshnessInfo {
                    state: FreshnessState::Live,
                    age_ms: 0,
                    sampled_at_ms: 0,
                    wake_required: false,
                },
            }),
            OperationRequest::GetOnboardingStatus => OperationResult::OnboardingStatus(OnboardingStatus {
                claimed: false,
                mode: self.runtime_mode(),
            }),
            OperationRequest::RequestWake { .. } => OperationResult::WakeStatus(WakeStatus {
                accepted: true,
                already_awake: true,
            }),
            OperationRequest::RequestTransition { .. }
            | OperationRequest::OpenSession
            | OperationRequest::CloseSession => OperationResult::Ack,
        }
    }

    #[cfg(feature = "transport_wifi")]
    pub async fn onboarding_progress(&self, storage: &Storage) -> Result<OnboardingProgress> {
        let state = self.load_onboarding_state(storage).await?;
        Ok(OnboardingProgress::from_state(&state))
    }

    #[cfg(feature = "transport_wifi")]
    pub async fn onboarding_state(&self, storage: &Storage) -> Result<PersistedOnboardingState> {
        self.load_onboarding_state(storage).await
    }

    #[cfg(feature = "transport_wifi")]
    pub async fn is_onboarded(&self, storage: &Storage) -> Result<bool> {
        Ok(self.load_onboarding_state(storage).await?.onboarded)
    }

    #[cfg(feature = "transport_wifi")]
    pub async fn create_onboarding_user(
        &self,
        storage: &Storage,
        auth: &AuthStore,
        input: &OnboardingUserInput,
    ) -> Result<CreateOnboardingUserOutcome> {
        let current = self.load_onboarding_state(storage).await?;
        if current.step_done("user") || current.user.is_some() {
            return Ok(CreateOnboardingUserOutcome::UserStepAlreadyDone);
        }

        let next = storage
            .update_onboarding(|current| {
                current.user = Some(StoredUser {
                    name: input.name.clone(),
                    username: input.username.clone(),
                    password: input.password.clone(),
                    language: input.language.clone(),
                });
                current.language = Some(input.language.clone());
                current.done.push("user".into());
                Ok(())
            })
            .await?;

        self.sync_runtime_mode_from_onboarding(&next);

        auth.save_user(&AuthUser {
            name: input.name.clone(),
            username: input.username.clone(),
            password: input.password.clone(),
            language: input.language.clone(),
        })
        .await?;

        Ok(CreateOnboardingUserOutcome::Created)
    }

    #[cfg(feature = "transport_wifi")]
    pub async fn complete_onboarding_core_config(
        &self,
        storage: &Storage,
        input: &OnboardingCoreConfigInput,
    ) -> Result<CompleteCoreConfigOutcome> {
        let current = self.load_onboarding_state(storage).await?;
        if current.step_done("core_config") {
            return Ok(CompleteCoreConfigOutcome::CoreConfigStepAlreadyDone);
        }
        if !current.step_done("user") {
            return Ok(CompleteCoreConfigOutcome::UserStepRequired);
        }

        let next = storage
            .update_onboarding(|current| {
                current.location_name = input.location_name.clone();
                current.country = input.country.clone();
                current.language = input
                    .language
                    .clone()
                    .or_else(|| current.language.clone());
                current.time_zone = input.time_zone.clone();
                current.unit_system = input.unit_system.clone();
                current.done.push("core_config".into());
                current.onboarded = current.step_done("user") && current.step_done("core_config");
                Ok(())
            })
            .await?;

        self.sync_runtime_mode_from_onboarding(&next);

        Ok(CompleteCoreConfigOutcome::Completed)
    }

    #[cfg(feature = "transport_wifi")]
    pub async fn complete_onboarding(
        &self,
        storage: &Storage,
    ) -> Result<PersistedOnboardingState> {
        let next = storage
            .update_onboarding(|current| {
                current.onboarded = true;
                current.done = vec!["user".into(), "core_config".into()];
                Ok(())
            })
            .await?;
        self.sync_runtime_mode_from_onboarding(&next);
        Ok(next)
    }

    #[cfg(feature = "transport_wifi")]
    pub async fn bootstrap_authorized_owner(
        &self,
        storage: &Storage,
        auth: &AuthStore,
        input: &AuthorizeBootstrapInput,
    ) -> Result<()> {
        let auth_user = AuthUser {
            name: input.display_name.clone(),
            username: input.username.clone(),
            password: input.password.clone(),
            language: input.language.clone(),
        };

        let next = storage
            .update_onboarding(|current| {
                current.user = Some(StoredUser::from(&auth_user));
                current.location_name = Some(input.location_name.clone());
                current.language = Some(input.language.clone());
                current.done = vec!["user".into(), "core_config".into()];
                current.onboarded = true;
                Ok(())
            })
            .await?;

        auth.save_user(&auth_user).await?;
        self.sync_runtime_mode_from_onboarding(&next);
        Ok(())
    }

    #[cfg(feature = "transport_wifi")]
    pub async fn auth_user(
        &self,
        auth: &AuthStore,
        storage: &Storage,
    ) -> Result<Option<AuthUser>> {
        let onboarding = self.load_onboarding_state(storage).await?;
        self.sync_runtime_mode_from_onboarding(&onboarding);
        auth.load_user_with_legacy_fallback(storage).await
    }

    #[cfg(feature = "transport_wifi")]
    async fn load_onboarding_state(&self, storage: &Storage) -> Result<PersistedOnboardingState> {
        let state = storage.load_onboarding().await?;
        self.sync_runtime_mode_from_onboarding(&state);
        Ok(state)
    }

    #[cfg(feature = "transport_wifi")]
    fn sync_runtime_mode_from_onboarding(&self, onboarding: &PersistedOnboardingState) {
        self.set_runtime_mode(RuntimeMode::from_persisted_onboarding(onboarding.onboarded));
    }
}

impl Default for AppCore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_has_expected_default_mode() {
        #[cfg(feature = "transport_wifi")]
        assert_eq!(RuntimeMode::default_for_build(), RuntimeMode::UnprovisionedWifi);

        #[cfg(feature = "transport_ble")]
        assert_eq!(RuntimeMode::default_for_build(), RuntimeMode::UnprovisionedBle);
    }

    #[test]
    fn app_core_resolves_policy_for_current_mode() {
        let core = AppCore::new();
        let policy = core.transport_policy();

        #[cfg(feature = "transport_wifi")]
        {
            assert_eq!(policy.compatibility_policy, CompatibilityPolicy::WifiHaCompat);
            assert_eq!(policy.auth_policy, AuthPolicy::OnboardingClaim);
        }

        #[cfg(feature = "transport_ble")]
        {
            assert_eq!(policy.compatibility_policy, CompatibilityPolicy::BleCompactProtocol);
            assert_eq!(policy.auth_policy, AuthPolicy::OnboardingClaim);
        }
    }

    #[test]
    fn persisted_onboarding_maps_to_operational_mode() {
        assert_eq!(
            RuntimeMode::from_persisted_onboarding(true),
            RuntimeMode::operational_for_build()
        );
        assert_eq!(
            RuntimeMode::from_persisted_onboarding(false),
            RuntimeMode::default_for_build()
        );
    }
}