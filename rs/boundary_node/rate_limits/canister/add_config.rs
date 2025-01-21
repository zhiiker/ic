use crate::{
    storage::StorableIncident,
    types::{self, AddConfigError, IncidentId, Timestamp},
};
use anyhow::{anyhow, Context};
use getrandom::getrandom;
use rate_limits_api as api;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

use crate::{
    state::CanisterApi,
    storage::{StorableConfig, StorableRule},
    types::{InputConfig, RuleId, Version},
};

pub const INIT_JSON_SCHEMA_VERSION: Version = 1;
pub const INIT_VERSION: Version = 1;

/// Defines a trait for adding new rate-limit configuration to the canister.
pub trait AddsConfig {
    /// # Arguments
    /// * `config` - new rate-limit configuration to be stored.
    /// * `time` - the timestamp indicating when the config is added.
    ///
    /// # Returns
    /// A result indicating success or a specific error
    fn add_config(&self, config: api::InputConfig, time: Timestamp) -> Result<(), AddConfigError>;
}

pub struct ConfigAdder<A> {
    /// The canister API used for interacting with the underlying storage
    pub canister_api: A,
}

impl<A> ConfigAdder<A> {
    pub fn new(canister_api: A) -> Self {
        Self { canister_api }
    }
}

// Definitions:
// - A rate-limit config is an ordered set of rate-limit rules: config = [rule_1, rule_2, ..., rule_N].
// - Rules order within a config is significant, as rules are applied in the order they appear in the config.
// - Adding a new config requires providing an entire list of ordered rules; config version is increment by one for 'add' operation.
// - Each rule is identified by its unique ID and its non-mutable context provided by the caller:
//   - `incident_id`: each rule must be linked to a certain incident_id; multiple rules can be linked to the same incident_id
//   - `rule_raw`: binary encoded JSON of the rate-limit rule
//   - `description`: some info why this rule was introduced
// - Alongside an immutable context, each rule includes metadata for a better auditability experience:
//   - `disclosed_at`: a timestamp at which the rule became publicly accessible for viewing
//   - `added_in_version`: config version in which the rule was first introduced (rule can persist across multiple config versions, if resubmitted)
//   - `removed_in_version`: config version in which the rule was removed, this happens when a rule in the current config is not resubmitted
// - The canister generates a unique, random ID for each newly submitted rule.
// - Individual rules or incidents (a set of rules sharing the same incident_id) can be disclosed. This implies that the context of the rule becomes visible for the callers with `RestrictedRead` access level.
// - Disclosing rules or incidents multiple times has no additional effect.

// Policies:
// - Immutability: rule's context (incident_id, rule_raw, description) cannot be modified. Hence an operation of resubmitting a "removed" rule would result in creation of a rule with a new ID, generated by the canister.
// - New rules cannot be linked to already disclosed incidents (LinkingRuleToDisclosedIncident error).

impl<A: CanisterApi> AddsConfig for ConfigAdder<A> {
    fn add_config(
        &self,
        input_config: api::InputConfig,
        time: Timestamp,
    ) -> Result<(), AddConfigError> {
        // Convert config from api type (also performs validation of each rule)
        let next_config = types::InputConfig::try_from(input_config)?;

        let current_version = self
            .canister_api
            .get_version()
            // this error indicates that canister was not initialized correctly
            .ok_or_else(|| AddConfigError::Internal(anyhow!("No existing config version found")))?;

        let current_config: StorableConfig = self
            .canister_api
            .get_config(current_version)
            .ok_or_else(|| {
                // this error indicates that canister was not initialized correctly
                AddConfigError::Internal(anyhow!("No config for version={current_version} found"))
            })?;

        let current_full_config: InputConfig = self
            .canister_api
            .get_full_config(current_version)
            .ok_or_else(|| {
            // this error indicates that canister was not initialized correctly
            AddConfigError::Internal(anyhow!("No config for version={current_version} found"))
        })?;

        let next_version = current_version.checked_add(1).ok_or_else(|| {
            AddConfigError::Internal(anyhow!(
                "Overflow occurred while incrementing the current version {current_version}"
            ))
        })?;

        // Ordered IDs of all rules in the submitted config
        let mut rule_ids = Vec::<RuleId>::new();
        // Newly submitted rules
        let mut new_rules = Vec::<(RuleId, StorableRule)>::new();
        // Hashmap of the submitted incident IDs
        let mut incidents_map = HashMap::<IncidentId, HashSet<RuleId>>::new();

        for (rule_idx, input_rule) in next_config.rules.iter().enumerate() {
            // Check if the rule is resubmitted or if it is a new rule
            let existing_rule_idx = current_full_config
                .rules
                .iter()
                .position(|rule| rule == input_rule);

            let rule_id = if let Some(rule_idx) = existing_rule_idx {
                current_config.rule_ids[rule_idx]
            } else {
                let rule_id = RuleId(generate_random_uuid()?);
                // If the generated UUID already exists, return the error (practically this should never happen).
                if self.canister_api.get_rule(&rule_id).is_some() {
                    return Err(AddConfigError::Internal(anyhow!(
                        "Failed to generate a new uuid {rule_id}, please retry the operation."
                    )));
                }

                // Check if the new rule is linked to an existing incident
                let existing_incident = self.canister_api.get_incident(&input_rule.incident_id);

                if let Some(incident) = existing_incident {
                    // A new rule can't be linked to a disclosed incident
                    if incident.is_disclosed {
                        Err(AddConfigError::LinkingRuleToDisclosedIncident {
                            index: rule_idx,
                            incident_id: input_rule.incident_id,
                        })?;
                    }
                }

                let rule = StorableRule {
                    incident_id: input_rule.incident_id,
                    rule_raw: input_rule.rule_raw.clone(),
                    description: input_rule.description.clone(),
                    disclosed_at: None,
                    added_in_version: next_version,
                    removed_in_version: None,
                };

                new_rules.push((rule_id, rule));

                rule_id
            };

            incidents_map
                .entry(input_rule.incident_id)
                .or_default()
                .insert(rule_id);

            rule_ids.push(rule_id);
        }

        let removed_rule_ids = {
            let rule_ids_set: HashSet<RuleId> = HashSet::from_iter(rule_ids.clone());
            current_config
                .rule_ids
                .into_iter()
                .filter(|&rule_id| !rule_ids_set.contains(&rule_id))
                .collect()
        };

        let storable_config = StorableConfig {
            schema_version: next_config.schema_version,
            active_since: time,
            rule_ids,
        };

        commit_changes(
            &self.canister_api,
            next_version,
            storable_config,
            removed_rule_ids,
            new_rules,
            incidents_map,
        );

        Ok(())
    }
}

fn generate_random_uuid() -> Result<Uuid, anyhow::Error> {
    let mut buf = [0u8; 16];
    getrandom(&mut buf)
        .map_err(|e| anyhow::anyhow!(e))
        .context("Failed to generate random bytes")?;
    let uuid = Uuid::from_slice(&buf).context("Failed to create UUID from bytes")?;
    Ok(uuid)
}

fn commit_changes(
    canister_api: &impl CanisterApi,
    next_version: u64,
    storable_config: StorableConfig,
    removed_rules: Vec<RuleId>,
    new_rules: Vec<(RuleId, StorableRule)>,
    incidents_map: HashMap<IncidentId, HashSet<RuleId>>,
) {
    // Update metadata of the removed rules in the stable memory
    for rule_id in removed_rules {
        // Rule should exist, it was already checked before.
        let mut rule = canister_api
            .get_rule(&rule_id)
            .expect("inconsistent state, rule_id={rule_id} not found");
        rule.removed_in_version = Some(next_version);
        canister_api.upsert_rule(rule_id, rule);
    }

    // Add new rules to the stable memory
    for (rule_id, rule) in new_rules {
        canister_api.upsert_rule(rule_id, rule);
    }

    // Upsert incidents to the stable memory, some of the incidents can be new, some already existed before
    for (incident_id, rule_ids) in incidents_map {
        let incident = canister_api
            .get_incident(&incident_id)
            .map(|mut stored_incident| {
                stored_incident.rule_ids.extend(rule_ids.clone());
                stored_incident
            })
            .unwrap_or_else(|| StorableIncident {
                is_disclosed: false,
                rule_ids: rule_ids.clone(),
            });

        canister_api.upsert_incident(incident_id, incident);
    }

    // Add a new config to the stable memory
    canister_api.add_config(next_version, storable_config);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::CanisterState;
    use rate_limits_api as api;
    use types::InputConfigError;

    #[derive(Debug, PartialEq)]
    struct FullConfig {
        schema_version: api::SchemaVersion,
        active_since: api::Timestamp,
        rules: Vec<StorableRule>,
    }

    fn retrieve_full_config(canister_api: impl CanisterApi, version: u64) -> FullConfig {
        let config = canister_api.get_config(version).unwrap();

        let mut full_config = FullConfig {
            schema_version: config.schema_version,
            active_since: config.active_since,
            rules: vec![],
        };

        for rule_id in config.rule_ids.iter() {
            let rule = canister_api.get_rule(rule_id).unwrap();
            full_config.rules.push(rule);
        }

        full_config
    }

    // A comprehensive test for adding new rate-limit configs
    #[test]
    fn test_add_config_success() {
        let current_time = 10u64;
        let schema_version = 1;
        let canister_state = CanisterState::from_static();
        // Add init config_1 corresponding to version=1 to the canister state
        canister_state.add_config(
            1,
            StorableConfig {
                schema_version,
                active_since: current_time,
                rule_ids: vec![],
            },
        );

        let incident_id_1 = IncidentId(Uuid::new_v4());
        let incident_id_2 = IncidentId(Uuid::new_v4());
        let incident_id_3 = IncidentId(Uuid::new_v4());

        // Two rules are added to the previous config.
        let config_2 = api::InputConfig {
            schema_version,
            rules: vec![
                api::InputRule {
                    incident_id: incident_id_1.0.to_string(),
                    rule_raw: b"{\"a\": 1, \"b\": 2}".to_vec(),
                    description: "best rule #1 ever".to_string(),
                },
                api::InputRule {
                    incident_id: incident_id_1.0.to_string(),
                    rule_raw: b"{\"c\": 3, \"d\": 4}".to_vec(),
                    description: "best rule #2 ever".to_string(),
                },
            ],
        };
        // Two rules are swapped.
        let config_3 = api::InputConfig {
            schema_version: schema_version + 1,
            rules: vec![
                api::InputRule {
                    incident_id: incident_id_1.0.to_string(),
                    rule_raw: b"{\"c\": 3, \"d\": 4}".to_vec(),
                    description: "best rule #2 ever".to_string(),
                },
                api::InputRule {
                    incident_id: incident_id_1.0.to_string(),
                    rule_raw: b"{\"a\": 1, \"b\": 2}".to_vec(),
                    description: "best rule #1 ever".to_string(),
                },
            ],
        };
        // One rule is added in the middle.
        // NOTE: rule binary representations of two rules have changed, but not the inner JSON, hence rules are unchanged
        let config_4 = api::InputConfig {
            schema_version: schema_version + 1,
            rules: vec![
                api::InputRule {
                    incident_id: incident_id_1.0.to_string(),
                    // binary representation has changed, but not the inner JSON
                    rule_raw: b"{\"b\": 2, \"a\": 1}".to_vec(),
                    description: "best rule #1 ever".to_string(),
                },
                // Brand new rule
                api::InputRule {
                    incident_id: incident_id_2.0.to_string(),
                    rule_raw: b"{}".to_vec(),
                    description: "best rule #3 ever".to_string(),
                },
                api::InputRule {
                    incident_id: incident_id_1.0.to_string(),
                    // binary representation has changed, but not the inner JSON
                    rule_raw: b"{\"d\": 4, \"c\": 3}".to_vec(),
                    description: "best rule #2 ever".to_string(),
                },
            ],
        };
        // This config adds two new rules and removes the first two.
        let config_5 = api::InputConfig {
            schema_version: schema_version + 1,
            // New rule, as incident id and description changed
            rules: vec![
                // Brand new rule
                api::InputRule {
                    incident_id: incident_id_2.0.to_string(),
                    rule_raw: b"{\"e\": 5, \"f\": 6}".to_vec(),
                    description: "best rate-limit rule #4 ever".to_string(),
                },
                // This rule is unchanged from the previous config
                api::InputRule {
                    incident_id: incident_id_1.0.to_string(),
                    rule_raw: b"{\"c\": 3, \"d\": 4}".to_vec(),
                    description: "best rule #2 ever".to_string(),
                },
                // Brand new rule
                api::InputRule {
                    incident_id: incident_id_3.0.to_string(),
                    rule_raw: b"{\"g\": 7, \"e\": 8}".to_vec(),
                    description: "best rate-limit rule #5 ever".to_string(),
                },
            ],
        };
        // This config removes all existing rules, it is empty
        let config_6 = api::InputConfig {
            schema_version: schema_version + 1,
            rules: vec![],
        };

        let adder = ConfigAdder::new(canister_state.clone());

        // Perform multiple add config operations
        adder
            .add_config(config_2.clone(), current_time)
            .expect("failed to add config");
        adder
            .add_config(config_3.clone(), current_time + 1)
            .expect("failed to add config");
        adder
            .add_config(config_4, current_time + 2)
            .expect("failed to add config");
        adder
            .add_config(config_5, current_time + 3)
            .expect("failed to add config");
        adder
            .add_config(config_6, current_time + 4)
            .expect("failed to add config");

        // Assert expected configs
        let version = 1;
        assert_eq!(
            retrieve_full_config(canister_state.clone(), version),
            FullConfig {
                schema_version: 1,
                active_since: current_time,
                rules: vec![],
            }
        );

        let version = 2;
        assert_eq!(
            retrieve_full_config(canister_state.clone(), version),
            FullConfig {
                schema_version: 1,
                active_since: current_time,
                rules: vec![
                    StorableRule {
                        incident_id: incident_id_1,
                        rule_raw: b"{\"a\": 1, \"b\": 2}".to_vec(),
                        description: "best rule #1 ever".to_string(),
                        disclosed_at: None,
                        added_in_version: 2,
                        removed_in_version: Some(5),
                    },
                    StorableRule {
                        incident_id: incident_id_1,
                        rule_raw: b"{\"c\": 3, \"d\": 4}".to_vec(),
                        description: "best rule #2 ever".to_string(),
                        disclosed_at: None,
                        added_in_version: 2,
                        removed_in_version: Some(6),
                    }
                ],
            }
        );

        let version = 3;
        assert_eq!(
            retrieve_full_config(canister_state.clone(), version),
            FullConfig {
                schema_version: 2,
                active_since: current_time + 1,
                rules: vec![
                    StorableRule {
                        incident_id: incident_id_1,
                        rule_raw: b"{\"c\": 3, \"d\": 4}".to_vec(),
                        description: "best rule #2 ever".to_string(),
                        disclosed_at: None,
                        added_in_version: 2,
                        removed_in_version: Some(6),
                    },
                    StorableRule {
                        incident_id: incident_id_1,
                        rule_raw: b"{\"a\": 1, \"b\": 2}".to_vec(),
                        description: "best rule #1 ever".to_string(),
                        disclosed_at: None,
                        added_in_version: 2,
                        removed_in_version: Some(5),
                    },
                ],
            }
        );

        let version = 4;
        assert_eq!(
            retrieve_full_config(canister_state.clone(), version),
            FullConfig {
                schema_version: 2,
                active_since: current_time + 2,
                rules: vec![
                    StorableRule {
                        incident_id: incident_id_1,
                        rule_raw: b"{\"a\": 1, \"b\": 2}".to_vec(),
                        description: "best rule #1 ever".to_string(),
                        disclosed_at: None,
                        added_in_version: 2,
                        removed_in_version: Some(5),
                    },
                    StorableRule {
                        incident_id: incident_id_2,
                        rule_raw: b"{}".to_vec(),
                        description: "best rule #3 ever".to_string(),
                        disclosed_at: None,
                        added_in_version: 4,
                        removed_in_version: Some(5),
                    },
                    StorableRule {
                        incident_id: incident_id_1,
                        rule_raw: b"{\"c\": 3, \"d\": 4}".to_vec(),
                        description: "best rule #2 ever".to_string(),
                        disclosed_at: None,
                        added_in_version: 2,
                        removed_in_version: Some(6),
                    }
                ],
            }
        );

        let version = 5;
        assert_eq!(
            retrieve_full_config(canister_state.clone(), version),
            FullConfig {
                schema_version: 2,
                active_since: current_time + 3,
                rules: vec![
                    StorableRule {
                        incident_id: incident_id_2,
                        rule_raw: b"{\"e\": 5, \"f\": 6}".to_vec(),
                        description: "best rate-limit rule #4 ever".to_string(),
                        disclosed_at: None,
                        added_in_version: 5,
                        removed_in_version: Some(6),
                    },
                    StorableRule {
                        incident_id: incident_id_1,
                        rule_raw: b"{\"c\": 3, \"d\": 4}".to_vec(),
                        description: "best rule #2 ever".to_string(),
                        disclosed_at: None,
                        added_in_version: 2,
                        removed_in_version: Some(6),
                    },
                    StorableRule {
                        incident_id: incident_id_3,
                        rule_raw: b"{\"g\": 7, \"e\": 8}".to_vec(),
                        description: "best rate-limit rule #5 ever".to_string(),
                        disclosed_at: None,
                        added_in_version: 5,
                        removed_in_version: Some(6),
                    },
                ],
            }
        );

        let version = 6;
        assert_eq!(
            retrieve_full_config(canister_state.clone(), version),
            FullConfig {
                schema_version: 2,
                active_since: current_time + 4,
                rules: vec![],
            }
        );

        assert_eq!(canister_state.incidents_count(), 3);
        assert_eq!(canister_state.active_rules_count(), 0);
        assert_eq!(canister_state.configs_count(), 6);
    }

    #[test]
    fn test_add_config_fails_with_invalid_inputs() {
        // Arrange
        let current_time = 10u64;
        let canister_state = CanisterState::from_static();
        let invalid_config_1 = api::InputConfig {
            schema_version: 1,
            rules: vec![api::InputRule {
                incident_id: "not_a_valid_uuid".to_string(),
                rule_raw: b"{}".to_vec(),
                description: "".to_string(),
            }],
        };
        let invalid_config_2 = api::InputConfig {
            schema_version: 1,
            rules: vec![
                api::InputRule {
                    incident_id: "ebe7dbb1-63c9-420e-980d-eb0f8c20a9fb".to_string(),
                    rule_raw: b"{}".to_vec(),
                    description: "".to_string(),
                },
                api::InputRule {
                    incident_id: "ebe7dbb1-63c9-420e-980d-eb0f8c20a9fb".to_string(),
                    rule_raw: b"not_a_valid_json".to_vec(),
                    description: "".to_string(),
                },
            ],
        };
        let invalid_config_3 = api::InputConfig {
            schema_version: 1,
            rules: vec![
                // Rules at indices 0 and 2 are identical because they have matching field values, even though their rule_raw JSON objects have different binary representations
                api::InputRule {
                    incident_id: "ebe7dbb1-63c9-420e-980d-eb0f8c20a9fb".to_string(),
                    rule_raw: b"{\"a\": 1, \"b\": 2}".to_vec(),
                    description: "verbose description".to_string(),
                },
                api::InputRule {
                    incident_id: "ebe7dbb1-63c9-420e-980d-eb0f8c20a9fb".to_string(),
                    rule_raw: b"[]".to_vec(),
                    description: "".to_string(),
                },
                // identical to rule at index = 0, despite having different rule_raw bin representation
                api::InputRule {
                    incident_id: "ebe7dbb1-63c9-420e-980d-eb0f8c20a9fb".to_string(),
                    rule_raw: b"{\"b\": 2, \"a\": 1}".to_vec(),
                    description: "verbose description".to_string(),
                },
            ],
        };
        // Act & assert
        let adder = ConfigAdder::new(canister_state);
        let error = adder
            .add_config(invalid_config_1, current_time)
            .unwrap_err();
        assert!(
            matches!(error, AddConfigError::InvalidInputConfig(InputConfigError::InvalidIncidentUuidFormat(idx)) if idx == 0)
        );
        let error = adder
            .add_config(invalid_config_2, current_time)
            .unwrap_err();
        assert!(
            matches!(error, AddConfigError::InvalidInputConfig(InputConfigError::InvalidRuleJsonEncoding(idx)) if idx == 1)
        );
        let error = adder
            .add_config(invalid_config_3, current_time)
            .unwrap_err();
        assert!(
            matches!(error, AddConfigError::InvalidInputConfig(InputConfigError::DuplicateRules(idx1, idx2))  if idx1 == 0 && idx2 == 2)
        );
    }

    #[test]
    fn test_add_config_without_init_version_fails() {
        // Arrange
        let canister_state = CanisterState::from_static();
        let adder = ConfigAdder::new(canister_state);
        let current_time = 10u64;
        let config = api::InputConfig {
            schema_version: 1,
            rules: vec![],
        };
        // Act & assert
        let error = adder.add_config(config, current_time).unwrap_err();
        assert!(
            matches!(error, AddConfigError::Internal(err) if err.to_string() == "No existing config version found")
        );
    }

    #[test]
    fn test_add_config_with_policy_violation_fails() {
        // Arrange
        let canister_state = CanisterState::from_static();
        canister_state.add_config(
            1,
            StorableConfig {
                schema_version: 1,
                active_since: 1,
                rule_ids: vec![],
            },
        );
        let incident_id_1 = IncidentId(Uuid::new_v4());
        let incident_id_2 = IncidentId(Uuid::new_v4());
        let storable_incident_1 = StorableIncident {
            is_disclosed: false,
            rule_ids: HashSet::new(),
        };
        // This incident is disclosed, new rules can't be linked to it anymore. \
        let storable_incident_2 = StorableIncident {
            is_disclosed: true,
            rule_ids: HashSet::new(),
        };
        canister_state.upsert_incident(incident_id_1, storable_incident_1);
        canister_state.upsert_incident(incident_id_2, storable_incident_2);
        let config = api::InputConfig {
            schema_version: 1,
            rules: vec![
                api::InputRule {
                    incident_id: incident_id_1.0.to_string(),
                    rule_raw: b"{}".to_vec(),
                    description: "".to_string(),
                },
                api::InputRule {
                    incident_id: incident_id_2.0.to_string(), // this incident is already disclosed, new rule can't be added
                    rule_raw: b"{}".to_vec(),
                    description: "".to_string(),
                },
            ],
        };
        // Act & assert
        let adder = ConfigAdder::new(canister_state);
        let error = adder.add_config(config, 1u64).unwrap_err();
        assert!(
            matches!(error, AddConfigError::LinkingRuleToDisclosedIncident{index, incident_id} if index == 1 && incident_id == incident_id_2)
        );
    }
}
