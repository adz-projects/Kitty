import numpy as np
from dataclasses import dataclass, field
from enum import Enum
from typing import Optional, Any

class AnnotationType(str, Enum):
    KEEP_THIS = "keep_this"
    DONT_DO_AGAIN = "dont_do_again"
    MICRO_POSITIVE = "micro_positive"
    MICRO_NEGATIVE = "micro_negative"
    EXPLORE_ALTERNATIVE = "explore_alternative"
    RETRY_SAME_INTENT = "retry_same_intent"

class DetectionMethod(str, Enum):
    EMBEDDING = "embedding"
    BEHAVIORAL = "behavioral"
    HYBRID = "hybrid"
    HEURISTIC = "heuristic"

class SchismState(str, Enum):
    NONE = "none"
    DETECTED = "detected"
    REVIEWING = "reviewing"
    RESOLVED = "resolved"

class EdgeStatus(str, Enum):
    PROVISIONAL = "provisional"
    ESTABLISHED = "established"

class DomainSource(str, Enum):
    AUTO_NAMED = "auto_named"
    USER_NAMED = "user_named"

class PrimitiveSource(str, Enum):
    AUTO_NAMED = "auto_named"
    USER_NAMED = "user_named"

@dataclass
class Action:
    id: str
    features: np.ndarray
    metadata: dict[str, Any] = field(default_factory=dict)

@dataclass
class Hint:
    text: str
    confidence: float
    primitive: str
    domain: str
    attribution_id: str
    edge_id: Optional[str] = None
    domains: list[str] = field(default_factory=list)
    rationale: Optional[str] = None
    source_model: str = "standard"

@dataclass
class BlendedHint:
    text: str
    confidence: float
    source_primitive_a: str
    source_primitive_b: str
    attribution_id: str
    edge_id: Optional[str] = None

    @property
    def primitive(self) -> str:
        return self.source_primitive_a

    @property
    def domain(self) -> str:
        return ""

@dataclass
class PlateauRisk:
    score: float
    entropy_risk: float
    diversity_risk: float
    novelty_risk: float
    agreement_risk: float
    trend: str
    ig_weight: float

@dataclass
class ParadigmChallengeScore:
    score: float
    domain_isolation: float
    confidence_gap: float
    primitive_isolation: float
    novelty_persistence: float
    domain_absent: bool
    domain_under_confident: bool

@dataclass
class PlateauDetection:
    entropy: float
    top3_concentration: float
    is_plateau: bool
    dominant_actions: list[tuple[str, int]]

@dataclass
class NudgeStatus:
    active: bool
    multiplier: float
    reason: str
    turns_remaining: int

@dataclass
class NudgeOffer:
    multiplier: float
    duration_turns: int
    reason: str

@dataclass
class InSessionStatus:
    mix_weight: float
    call_count: int
    max_weight: float
    buffer_size: int

@dataclass
class SchismAlert:
    faction_a: list[int]
    faction_b: list[int]
    within_a: float
    within_b: float
    between: float
    faction_a_models: int
    faction_b_models: int
    detected_at: str

@dataclass
class DecisionResult:
    hints: list
    confidence: float
    novelty: float
    attribution_ids: list[str]
    is_flow_state: bool
    schism_alert: Optional[SchismAlert] = None
    plateau_risk: Optional[PlateauRisk] = None
    in_session: Optional[InSessionStatus] = None
    nudge_active: Optional[NudgeStatus] = None
    nudge_offered: bool = False
    exploration_metrics: Optional[dict] = None

@dataclass
class SessionState:
    session_id: str
    mode: str
    domain_hint: Optional[str] = None
    suggestions_paused: bool = False
    annotations_deferred: list = field(default_factory=list)
    notifications_deferred: list = field(default_factory=list)
    busy_session: bool = False
    opened_at: str = ""
    in_session: Any = None
    selector: Any = None
    last_hints: list = field(default_factory=list)
    co_selected: dict = field(default_factory=dict)
    wildcard_count: int = 0
    uncertainty_slot_count: int = 0
    micro_reward_used: float = 0.0
    # Domain inferred on the most recent `decide` call — `record_outcome` has
    # no domain input of its own, so newly-created edges inherit this.
    last_domain_id: Optional[str] = None

@dataclass
class DomainProfile:
    id: str
    name: str
    source: DomainSource = DomainSource.AUTO_NAMED
    dpp_diversity_weight: float = 1.0
    novelty_lambda: float = 0.5
    revision_rate: float = 0.0
    acceptance_rate: float = 0.0
    sessions: int = 0
    edge_count: int = 0
    override_rate: float = 0.0
    last_inferred: Optional[str] = None
    user_override: Optional[dict] = None
    locked: bool = False

@dataclass
class EdgeInfo:
    id: str
    semantic_primitive: str = ""
    domain: str = ""
    domain_id: str = ""
    confidence: float = 0.5
    status: EdgeStatus = EdgeStatus.PROVISIONAL
    tier: str = "hot"
    source: PrimitiveSource = PrimitiveSource.AUTO_NAMED
    frequency: int = 0
    override_rate: float = 0.0
    last_accessed: str = ""
    created_at: str = ""
    ttl: Optional[str] = None
    tags: list[str] = field(default_factory=list)
    domain_tags: list[str] = field(default_factory=list)
    embedding: Optional[np.ndarray] = None
    metadata: dict[str, Any] = field(default_factory=dict)
    co_selected_with: list[str] = field(default_factory=list)

@dataclass
class GraphHealth:
    total_edges: int = 0
    high_confidence_pct: float = 0.0
    flagged_hotspots: int = 0
    last_override_rate: float = 0.0
    blocking_issues: bool = False
    dimensionality_health: dict = field(default_factory=dict)
    ensemble_health: dict = field(default_factory=dict)
    novelty_health: dict = field(default_factory=dict)
    tier_distribution: dict = field(default_factory=dict)
    hotspot_details: list = field(default_factory=list)

@dataclass
class HealthIssue:
    severity: str
    component: str
    message: str
    details: dict = field(default_factory=dict)
