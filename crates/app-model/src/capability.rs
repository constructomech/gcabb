//! Capability reporting for inherited runtime features.
//!
//! Phase 3 requires that inherited capabilities are *proven* rather than
//! assumed. Every capability the self-hosting loop depends on is represented
//! here with an explicit status, so a missing GitHub token or a runtime that
//! stopped registering a tool becomes visible session state instead of an
//! unexplained model failure.

use serde::{Deserialize, Serialize};

use crate::tools::{ToolCatalog, ToolClass};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityStatus {
    /// Proven present through the SDK in this session.
    Available,
    /// Proven absent.
    Unavailable,
    /// Present but not usable, typically missing authentication.
    NeedsAttention,
    #[default]
    Unknown,
}

/// Stable identifiers for the capabilities the self-hosting loop needs.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityId {
    FileRead,
    FileWrite,
    Search,
    Shell,
    GithubMcp,
    Skills,
    Changes,
}

impl CapabilityId {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::FileRead => "File inspection",
            Self::FileWrite => "File editing",
            Self::Search => "Code search",
            Self::Shell => "Terminal commands",
            Self::GithubMcp => "GitHub MCP",
            Self::Skills => "Skills",
            Self::Changes => "Changes view",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Capability {
    pub id: CapabilityId,
    pub status: CapabilityStatus,
    /// Human-readable explanation, shown when status is not `Available`.
    pub detail: String,
    /// Runtime tool names that evidence this capability.
    #[serde(default)]
    pub evidence: Vec<String>,
}

impl Capability {
    #[must_use]
    pub fn unknown(id: CapabilityId) -> Self {
        Self {
            id,
            status: CapabilityStatus::Unknown,
            detail: "Not yet probed.".to_owned(),
            evidence: Vec::new(),
        }
    }

    #[must_use]
    pub const fn is_blocking(&self) -> bool {
        matches!(
            self.status,
            CapabilityStatus::Unavailable | CapabilityStatus::NeedsAttention
        )
    }
}

/// The full capability picture for a session.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CapabilityReport {
    #[serde(default)]
    pub capabilities: Vec<Capability>,
}

impl CapabilityReport {
    #[must_use]
    pub fn get(&self, id: CapabilityId) -> Option<&Capability> {
        self.capabilities
            .iter()
            .find(|capability| capability.id == id)
    }

    pub fn set(&mut self, capability: Capability) {
        if let Some(existing) = self
            .capabilities
            .iter_mut()
            .find(|existing| existing.id == capability.id)
        {
            *existing = capability;
        } else {
            self.capabilities.push(capability);
        }
    }

    /// Capabilities that would block the self-hosting loop.
    ///
    /// Scoped to the capabilities the loop actually requires. An absent MCP
    /// server or skill tool is worth showing in the capabilities panel, but it
    /// does not stop a developer editing, running commands, and reviewing
    /// diffs, so it must not be reported as blocking.
    #[must_use]
    pub fn blocking(&self) -> Vec<&Capability> {
        self.capabilities
            .iter()
            .filter(|capability| Self::is_required(capability.id) && capability.is_blocking())
            .collect()
    }

    /// Capabilities in a non-available state, whether required or not.
    #[must_use]
    pub fn degraded(&self) -> Vec<&Capability> {
        self.capabilities
            .iter()
            .filter(|capability| capability.status != CapabilityStatus::Available)
            .collect()
    }

    /// Whether a capability is required for the self-hosting loop.
    #[must_use]
    pub const fn is_required(id: CapabilityId) -> bool {
        matches!(
            id,
            CapabilityId::FileRead
                | CapabilityId::FileWrite
                | CapabilityId::Search
                | CapabilityId::Shell
                | CapabilityId::Changes
        )
    }

    #[must_use]
    pub fn is_self_hosting_ready(&self) -> bool {
        [
            CapabilityId::FileRead,
            CapabilityId::FileWrite,
            CapabilityId::Search,
            CapabilityId::Shell,
            CapabilityId::Changes,
        ]
        .into_iter()
        .all(|id| {
            self.get(id)
                .is_some_and(|capability| capability.status == CapabilityStatus::Available)
        })
    }

    /// Derive tool-backed capabilities from a discovered catalog.
    ///
    /// Only tool presence is inferred here. Capabilities that need a live
    /// operation to prove (GitHub MCP authentication, the changes view) are
    /// set by their owning services.
    #[must_use]
    pub fn from_catalog(catalog: &ToolCatalog) -> Self {
        let mut report = Self::default();
        if !catalog.is_discovered() {
            for id in [
                CapabilityId::FileRead,
                CapabilityId::FileWrite,
                CapabilityId::Search,
                CapabilityId::Shell,
                CapabilityId::GithubMcp,
                CapabilityId::Skills,
            ] {
                let mut capability = Capability::unknown(id);
                if let Some(error) = &catalog.error {
                    capability.status = CapabilityStatus::Unknown;
                    capability.detail = format!("Tool discovery failed: {error}");
                }
                report.set(capability);
            }
            return report;
        }

        report.set(tool_capability(
            catalog,
            CapabilityId::FileRead,
            ToolClass::reads_files,
            "No file inspection tool is registered by the runtime.",
        ));
        report.set(tool_capability(
            catalog,
            CapabilityId::FileWrite,
            ToolClass::writes_files,
            "No file editing tool is registered by the runtime.",
        ));
        report.set(tool_capability(
            catalog,
            CapabilityId::Search,
            |class| class == ToolClass::Search,
            "No code search tool is registered by the runtime.",
        ));
        report.set(tool_capability(
            catalog,
            CapabilityId::Shell,
            |class| class == ToolClass::Shell,
            "No shell tool is registered by the runtime.",
        ));

        let mcp_tools: Vec<String> = catalog
            .tools
            .iter()
            .filter(|tool| matches!(tool.source, crate::tools::ToolSource::Mcp { .. }))
            .map(|tool| tool.name.clone())
            .collect();
        report.set(Capability {
            id: CapabilityId::GithubMcp,
            status: if mcp_tools.is_empty() {
                CapabilityStatus::Unavailable
            } else {
                CapabilityStatus::Available
            },
            detail: if mcp_tools.is_empty() {
                "No MCP tools were discovered. Check GitHub authentication and MCP configuration."
                    .to_owned()
            } else {
                format!("{} MCP tools discovered.", mcp_tools.len())
            },
            evidence: mcp_tools,
        });

        let skill_tools: Vec<String> = catalog
            .by_class(ToolClass::Skill)
            .into_iter()
            .map(|tool| tool.name.clone())
            .collect();
        report.set(Capability {
            id: CapabilityId::Skills,
            status: if skill_tools.is_empty() {
                CapabilityStatus::Unavailable
            } else {
                CapabilityStatus::Available
            },
            detail: if skill_tools.is_empty() {
                "The runtime did not register a skill tool.".to_owned()
            } else {
                "Skill discovery is available.".to_owned()
            },
            evidence: skill_tools,
        });

        report
    }
}

fn tool_capability(
    catalog: &ToolCatalog,
    id: CapabilityId,
    matches: impl Fn(ToolClass) -> bool,
    missing_detail: &str,
) -> Capability {
    let evidence: Vec<String> = catalog
        .tools
        .iter()
        .filter(|tool| matches(tool.class))
        .map(|tool| tool.name.clone())
        .collect();
    if evidence.is_empty() {
        Capability {
            id,
            status: CapabilityStatus::Unavailable,
            detail: missing_detail.to_owned(),
            evidence,
        }
    } else {
        Capability {
            id,
            status: CapabilityStatus::Available,
            detail: format!("Provided by {}.", evidence.join(", ")),
            evidence,
        }
    }
}
