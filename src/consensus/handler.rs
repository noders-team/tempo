//! Consensus message handler that bridges Malachite consensus to the Reth application

use crate::{app::State, context::MalachiteContext};
use eyre::eyre;
use malachitebft_app_channel::{AppMsg, Channels, ConsensusMsg, NetworkMsg};
use malachitebft_core_types::{Height as _, Round, Validity};
use tracing::{error, info};

/// Run the consensus message handler loop
///
/// This function receives messages from the Malachite consensus engine and
/// delegates them to the appropriate methods on the application state.
pub async fn run_consensus_handler(
    state: &State,
    channels: &mut Channels<MalachiteContext>,
) -> eyre::Result<()> {
    while let Some(msg) = channels.consensus.recv().await {
        match msg {
            // Consensus is ready to start
            AppMsg::ConsensusReady { reply, .. } => {
                info!("Consensus engine is ready");

                // Determine the starting height
                let start_height = state.current_height()?;
                let validator_set = state.get_validator_set(start_height);

                if reply.send((start_height, validator_set)).is_err() {
                    error!("Failed to send ConsensusReady reply");
                }
            }

            // New round has started
            AppMsg::StartedRound {
                height,
                round,
                proposer,
                role,
                reply_value,
            } => {
                info!(%height, %round, %proposer, ?role, "Started new round");

                // Update state with current round info
                state.set_current_height(height)?;
                state.set_current_round(round)?;
                state.set_current_proposer(Some(proposer))?;
                // Convert malachitebft_app::consensus::Role to our app::Role
                let app_role = match role {
                    malachitebft_app::consensus::Role::Proposer => crate::app::Role::Proposer,
                    malachitebft_app::consensus::Role::Validator => crate::app::Role::Validator,
                    malachitebft_app::consensus::Role::None => crate::app::Role::None,
                };
                state.set_current_role(app_role)?;

                // Check if we have any pending proposals for this height/round
                let proposals = vec![]; // TODO: Query from state storage

                if reply_value.send(proposals).is_err() {
                    error!("Failed to send StartedRound reply");
                }
            }

            // Consensus requests a value to propose
            AppMsg::GetValue {
                height,
                round,
                timeout: _,
                reply,
            } => {
                info!(%height, %round, "Consensus requesting value to propose");

                // Check if we've already built a value for this height/round
                match state.get_previously_built_value(height, round).await? {
                    Some(proposal) => {
                        info!("Reusing previously built value");
                        if reply.send(proposal).is_err() {
                            error!("Failed to send GetValue reply");
                        }
                    }
                    None => {
                        // Build a new value
                        match state.propose_value(height, round).await {
                            Ok(proposal) => {
                                if reply.send(proposal.clone()).is_err() {
                                    error!("Failed to send GetValue reply");
                                }

                                // Stream the proposal parts to peers
                                for part in state.stream_proposal(proposal, Round::Nil) {
                                    channels
                                        .network
                                        .send(NetworkMsg::PublishProposalPart(part))
                                        .await?;
                                }
                            }
                            Err(e) => {
                                error!(%e, "Failed to build value");
                                // The channel will be closed on drop
                            }
                        }
                    }
                }
            }

            // Vote extension handling (not used for now)
            AppMsg::ExtendVote { reply, .. } => {
                if reply.send(None).is_err() {
                    error!("Failed to send ExtendVote reply");
                }
            }

            AppMsg::VerifyVoteExtension { reply, .. } => {
                if reply.send(Ok(())).is_err() {
                    error!("Failed to send VerifyVoteExtension reply");
                }
            }

            // Received a proposal part from another validator
            AppMsg::ReceivedProposalPart { from, part, reply } => {
                info!(%from, "Received proposal part");

                match state.received_proposal_part(from, part).await {
                    Ok(proposed_value) => {
                        if reply.send(proposed_value).is_err() {
                            error!("Failed to send ReceivedProposalPart reply");
                        }
                    }
                    Err(e) => {
                        error!(%e, "Failed to process proposal part");
                        if reply.send(None).is_err() {
                            error!("Failed to send ReceivedProposalPart reply");
                        }
                    }
                }
            }

            // Request for validator set at a specific height
            AppMsg::GetValidatorSet { height, reply } => {
                let validator_set = state.get_validator_set(height);
                if reply.send(Some(validator_set)).is_err() {
                    error!("Failed to send GetValidatorSet reply");
                }
            }

            // Consensus has decided on a value
            AppMsg::Decided {
                certificate,
                extensions,
                reply,
            } => {
                info!(
                    height = %certificate.height,
                    round = %certificate.round,
                    value = %certificate.value_id,
                    "Consensus decided on value"
                );

                // Commit the decided value
                match state.commit(certificate.clone(), extensions).await {
                    Ok(_) => {
                        // Move to next height
                        let current = state.current_height()?;
                        let next_height = current.increment();
                        state.set_current_height(next_height)?;

                        if reply
                            .send(ConsensusMsg::StartHeight(
                                next_height,
                                state.get_validator_set(next_height),
                            ))
                            .is_err()
                        {
                            error!("Failed to send StartHeight reply");
                        }
                    }
                    Err(e) => {
                        error!(%e, "Failed to commit decided value");
                        // Restart the current height
                        let current = state.current_height()?;
                        if reply
                            .send(ConsensusMsg::RestartHeight(
                                current,
                                state.get_validator_set(current),
                            ))
                            .is_err()
                        {
                            error!("Failed to send RestartHeight reply");
                        }
                    }
                }
            }

            // Process a synced value from another node
            AppMsg::ProcessSyncedValue {
                height,
                round,
                proposer,
                value_bytes,
                reply,
            } => {
                info!(%height, %round, "Processing synced value");

                if let Some(value) = crate::app::decode_value(value_bytes) {
                    let proposed_value = malachitebft_app_channel::app::types::ProposedValue {
                        height,
                        round,
                        valid_round: Round::Nil,
                        proposer,
                        value,
                        validity: Validity::Valid,
                    };

                    // Store the synced value
                    if let Err(e) = state.store_synced_proposal(proposed_value.clone()).await {
                        error!(%e, "Failed to store synced proposal");
                    }

                    if reply.send(Some(proposed_value)).is_err() {
                        error!("Failed to send ProcessSyncedValue reply");
                    }
                } else if reply.send(None).is_err() {
                    error!("Failed to send ProcessSyncedValue reply");
                }
            }

            // Request for a decided value at a specific height
            AppMsg::GetDecidedValue { height, reply } => {
                info!(%height, "Request for decided value");

                let decided_value = state.get_decided_value(height).await;
                let raw_value = decided_value.map(|dv| {
                    malachitebft_app_channel::app::types::sync::RawDecidedValue {
                        certificate: dv.certificate,
                        value_bytes: crate::app::encode_value(&dv.value),
                    }
                });

                if reply.send(raw_value).is_err() {
                    error!("Failed to send GetDecidedValue reply");
                }
            }

            // Request for the earliest available height
            AppMsg::GetHistoryMinHeight { reply } => {
                let min_height = state.get_earliest_height().await;
                if reply.send(min_height).is_err() {
                    error!("Failed to send GetHistoryMinHeight reply");
                }
            }

            // Request to restream a proposal
            AppMsg::RestreamProposal {
                height,
                round,
                valid_round,
                address: _,
                value_id,
            } => {
                info!(%height, %round, %valid_round, "Restreaming proposal");

                // Look for the proposal at valid_round or round
                let proposal_round = if valid_round == Round::Nil {
                    round
                } else {
                    valid_round
                };

                match state
                    .get_proposal_for_restreaming(height, proposal_round, value_id)
                    .await
                {
                    Ok(Some(proposal)) => {
                        let locally_proposed =
                            malachitebft_app_channel::app::types::LocallyProposedValue {
                                height,
                                round,
                                value: proposal.value,
                            };

                        // Stream the proposal parts
                        for part in state.stream_proposal(locally_proposed, valid_round) {
                            channels
                                .network
                                .send(NetworkMsg::PublishProposalPart(part))
                                .await?;
                        }
                    }
                    Ok(None) => {
                        info!("Proposal not found for restreaming");
                    }
                    Err(e) => {
                        error!(%e, "Failed to get proposal for restreaming");
                    }
                }
            }
        }
    }

    // Channel closed, consensus has stopped
    Err(eyre!("Consensus channel closed unexpectedly"))
}
