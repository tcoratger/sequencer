use std::fmt::Debug;

use async_trait::async_trait;
use futures::channel::{mpsc, oneshot};
use papyrus_protobuf::consensus::{ConsensusMessage, Vote};
use papyrus_protobuf::converters::ProtobufConversionError;
use starknet_api::block::{BlockHash, BlockNumber};
use starknet_api::core::ContractAddress;

/// Used to identify the node by consensus.
/// 1. This ID is derived from the id registered with Starknet's L2 staking contract.
/// 2. We must be able to derive the public key associated with this ID for the sake of validating
///    signatures.
// TODO(matan): Determine the actual type of NodeId.
pub type ValidatorId = ContractAddress;
pub type Round = u32;

/// Interface that any concrete block type must implement to be used by consensus.
///
/// In principle Consensus does not care about the content of a block. In practice though it will
/// need to perform certain activities with blocks:
/// 1. All proposals for a given height are held by consensus for book keeping, with only the
///    decided block returned to ConsensusContext.
/// 2. Tendermint may require re-broadcasting an old proposal [Line 16 of Algorithm 1](https://arxiv.org/pdf/1807.04938)
// This trait was designed with the following in mind:
// 1. It must allow `ConsensusContext` to be object safe. This precludes generics.
// 2. Starknet blocks are expected to be quite large, and we expect consensus to hold something akin
//    to a reference with a small stack size and cheap shallow cloning.
pub trait ConsensusBlock: Send {
    /// The chunks of content returned when iterating the proposal.
    // In practice I expect this to match the type sent to the network
    // (papyrus_protobuf::ConsensusMessage), and not to be specific to just the block's content.
    type ProposalChunk;
    /// Iterator for accessing the proposal's content.
    // An associated type is used instead of returning `impl Iterator` due to object safety.
    type ProposalIter: Iterator<Item = Self::ProposalChunk>;

    /// Identifies the block for the sake of Consensus voting.
    // The proposal's round must not be included in the ID, as, beyond being a detail of
    // consensus, Tendermint must be able to progress a value across multiple rounds of a given
    // height.
    //
    // Including a proposal's height in ID is optional from the perspective of consensus.
    // Since the proposal as well as votes sign not only on the block ID but also the height at
    // which they vote, not including height poses no security risk. Including it has no impact on
    // Tendermint.
    fn id(&self) -> BlockHash;

    /// Returns an iterator for streaming out this block as a proposal to other nodes.
    // Note on the ownership and lifetime model. This call is done by reference, yet the returned
    // iterator is implicitly an owning iterator.
    // 1. Why did we not want reference iteration? This would require a lifetime to be part of the
    //    type definition for `ProposalIter` and therefore `ConsensusBlock`. This results in a lot
    //    of lifetime pollution making it much harder to work with this type; attempted both options
    //    from here:
    //    https://stackoverflow.com/questions/33734640/how-do-i-specify-lifetime-parameters-in-an-associated-type
    // 2. Why is owning iteration reasonable? The expected use case for this is to stream out the
    //    proposal to other nodes, which implies ownership of data, not just a reference for
    //    internal use. We also expect the actual object implementing this trait to be itself a
    //    reference to the underlying data, and so returning an "owning" iterator to be relatively
    //    cheap.
    // TODO(matan): Consider changing ConsensusBlock to `IntoIterator + Clone` and removing
    // `proposal_iter`.
    fn proposal_iter(&self) -> Self::ProposalIter;
}

/// Interface for consensus to call out to the node.
#[async_trait]
pub trait ConsensusContext {
    /// The [block](`ConsensusBlock`) type built by `ConsensusContext` from a proposal.
    // We use an associated type since consensus is indifferent to the actual content of a proposal,
    // but we cannot use generics due to object safety.
    type Block: ConsensusBlock;

    // TODO(matan): The oneshot for receiving the build block could be generalized to just be some
    // future which returns a block.

    /// This function is called by consensus to request a block from the node. It expects that this
    /// call will return immediately and that consensus can then stream in the block's content in
    /// parallel to the block being built.
    ///
    /// Params:
    /// - `height`: The height of the block to be built. Specifically this indicates the initial
    ///   state of the block.
    ///
    /// Returns:
    /// - A receiver for the stream of the block's content.
    /// - A receiver for the fully built block once ConsensusContext has finished streaming out the
    ///   content and building it. If the block fails to be built, the Sender will be dropped by
    ///   ConsensusContext.
    async fn build_proposal(
        &self,
        height: BlockNumber,
    ) -> (
        mpsc::Receiver<<Self::Block as ConsensusBlock>::ProposalChunk>,
        oneshot::Receiver<Self::Block>,
    );

    /// This function is called by consensus to validate a block. It expects that this call will
    /// return immediately and that context can then stream in the block's content in parallel to
    /// consensus continuing to handle other tasks.
    ///
    /// Params:
    /// - `height`: The height of the block to be built. Specifically this indicates the initial
    ///   state of the block.
    /// - A receiver for the stream of the block's content.
    ///
    /// Returns:
    /// - A receiver for the fully built block. If a valid block cannot be built the Sender will be
    ///   dropped by ConsensusContext.
    async fn validate_proposal(
        &self,
        height: BlockNumber,
        content: mpsc::Receiver<<Self::Block as ConsensusBlock>::ProposalChunk>,
    ) -> oneshot::Receiver<Self::Block>;

    /// Get the set of validators for a given height. These are the nodes that can propose and vote
    /// on blocks.
    // TODO(matan): We expect this to change in the future to BTreeMap. Why?
    // 1. Map - The nodes will have associated information (e.g. voting weight).
    // 2. BTreeMap - We want a stable ordering of the nodes for deterministic leader selection.
    async fn validators(&self, height: BlockNumber) -> Vec<ValidatorId>;

    /// Calculates the ID of the Proposer based on the inputs.
    fn proposer(&self, height: BlockNumber, round: Round) -> ValidatorId;

    async fn broadcast(&mut self, message: ConsensusMessage) -> Result<(), ConsensusError>;

    /// This should be non-blocking. Meaning it returns immediately and waits to receive from the
    /// input channels in parallel (ie on a separate task).
    // TODO(matan): change to just be a generic broadcast function.
    async fn propose(
        &self,
        init: ProposalInit,
        content_receiver: mpsc::Receiver<<Self::Block as ConsensusBlock>::ProposalChunk>,
        fin_receiver: oneshot::Receiver<BlockHash>,
    ) -> Result<(), ConsensusError>;

    /// Update the context that a decision has been reached for a given height.
    /// - `block` identifies the decision.
    /// - `precommits` - All precommits must be for the same `(block.id(), height, round)` and form
    ///   a quorum (>2/3 of the voting power) for this height.
    async fn decision_reached(
        &mut self,
        block: Self::Block,
        precommits: Vec<Vote>,
    ) -> Result<(), ConsensusError>;
}

#[derive(PartialEq)]
pub struct Decision<BlockT: ConsensusBlock> {
    pub precommits: Vec<Vote>,
    pub block: BlockT,
}

impl<BlockT: ConsensusBlock> Debug for Decision<BlockT> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Decision")
            .field("block_id", &self.block.id())
            .field("precommits", &self.precommits)
            .finish()
    }
}

#[derive(PartialEq, Debug, Clone)]
pub struct ProposalInit {
    pub height: BlockNumber,
    pub round: Round,
    pub proposer: ValidatorId,
}

#[derive(thiserror::Error, PartialEq, Debug)]
pub enum ConsensusError {
    #[error(transparent)]
    Canceled(#[from] oneshot::Canceled),
    #[error(transparent)]
    ProtobufConversionError(#[from] ProtobufConversionError),
    /// This should never occur, since events are internally generated.
    #[error("Invalid event: {0}")]
    InvalidEvent(String),
    #[error("Invalid proposal sent by peer {0:?} at height {1}: {2}")]
    InvalidProposal(ValidatorId, BlockNumber, String),
    #[error(transparent)]
    SendError(#[from] mpsc::SendError),
    #[error("Conflicting messages for block {0}. Old: {1:?}, New: {2:?}")]
    Equivocation(BlockNumber, ConsensusMessage, ConsensusMessage),
    // Indicates an error in communication between consensus and the node's networking component.
    // As opposed to an error between this node and peer nodes.
    #[error("{0}")]
    InternalNetworkError(String),
    #[error("{0}")]
    SyncError(String),
}
