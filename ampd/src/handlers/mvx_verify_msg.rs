use std::collections::HashSet;
use std::convert::TryInto;

use async_trait::async_trait;
use cosmrs::cosmwasm::MsgExecuteContract;
use cosmrs::{tx::Msg, Any};
use error_stack::ResultExt;
use multiversx_sdk::data::address::Address;
use serde::Deserialize;
use tokio::sync::watch::Receiver;
use tracing::info;

use axelar_wasm_std::voting::{PollId, Vote};
use events::Error::EventTypeMismatch;
use events::Event;
use events_derive::try_from;
use voting_verifier::msg::ExecuteMsg;

use crate::event_processor::EventHandler;
use crate::handlers::errors::Error;
use crate::mvx::proxy::MvxProxy;
use crate::mvx::verifier::verify_message;
use crate::types::{Hash, TMAddress};

type Result<T> = error_stack::Result<T, Error>;

#[derive(Deserialize, Debug)]
pub struct Message {
    pub tx_id: Hash,
    pub event_index: u32,
    pub destination_address: String,
    pub destination_chain: router_api::ChainName,
    pub source_address: Address,
    pub payload_hash: Hash,
}

#[derive(Deserialize, Debug)]
#[try_from("wasm-messages_poll_started")]
struct PollStartedEvent {
    poll_id: PollId,
    source_gateway_address: Address,
    messages: Vec<Message>,
    participants: Vec<TMAddress>,
    expires_at: u64,
}

pub struct Handler<P>
where
    P: MvxProxy + Send + Sync,
{
    verifier: TMAddress,
    voting_verifier_contract: TMAddress,
    blockchain: P,
    latest_block_height: Receiver<u64>,
}

impl<P> Handler<P>
where
    P: MvxProxy + Send + Sync,
{
    pub fn new(
        verifier: TMAddress,
        voting_verifier_contract: TMAddress,
        blockchain: P,
        latest_block_height: Receiver<u64>,
    ) -> Self {
        Self {
            verifier,
            voting_verifier_contract,
            blockchain,
            latest_block_height,
        }
    }

    fn vote_msg(&self, poll_id: PollId, votes: Vec<Vote>) -> MsgExecuteContract {
        MsgExecuteContract {
            sender: self.verifier.as_ref().clone(),
            contract: self.voting_verifier_contract.as_ref().clone(),
            msg: serde_json::to_vec(&ExecuteMsg::Vote { poll_id, votes })
                .expect("vote msg should serialize"),
            funds: vec![],
        }
    }
}

#[async_trait]
impl<P> EventHandler for Handler<P>
where
    P: MvxProxy + Send + Sync,
{
    type Err = Error;

    async fn handle(&self, event: &Event) -> Result<Vec<Any>> {
        if !event.is_from_contract(self.voting_verifier_contract.as_ref()) {
            return Ok(vec![]);
        }

        let PollStartedEvent {
            poll_id,
            source_gateway_address,
            messages,
            participants,
            expires_at,
            ..
        } = match event.try_into() as error_stack::Result<_, _> {
            Err(report) if matches!(report.current_context(), EventTypeMismatch(_)) => {
                return Ok(vec![]);
            }
            event => event.change_context(Error::DeserializeEvent)?,
        };

        if !participants.contains(&self.verifier) {
            return Ok(vec![]);
        }

        let latest_block_height = *self.latest_block_height.borrow();
        if latest_block_height >= expires_at {
            info!(poll_id = poll_id.to_string(), "skipping expired poll");

            return Ok(vec![]);
        }

        let tx_hashes: HashSet<_> = messages
            .iter()
            .map(|message| message.tx_id.clone())
            .collect();
        let transactions_info = self
            .blockchain
            .transactions_info_with_results(tx_hashes)
            .await
            .change_context(Error::TxReceipts)?;

        let votes: Vec<Vote> = messages
            .iter()
            .map(|msg| {
                transactions_info
                    .get(&msg.tx_id)
                    .map_or(Vote::NotFound, |transaction| {
                        verify_message(&source_gateway_address, transaction, msg)
                    })
            })
            .collect();

        Ok(vec![self
            .vote_msg(poll_id, votes)
            .into_any()
            .expect("vote msg should serialize")])
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::convert::TryInto;

    use cosmrs::cosmwasm::MsgExecuteContract;
    use cosmrs::tx::Msg;
    use cosmwasm_std;
    use error_stack::{Report, Result};
    use tokio::sync::watch;
    use tokio::test as async_test;

    use voting_verifier::events::{PollMetadata, PollStarted, TxEventConfirmation};

    use crate::event_processor::EventHandler;
    use crate::handlers::errors::Error;
    use crate::handlers::tests::get_event;
    use crate::mvx::proxy::MockMvxProxy;
    use crate::types::{EVMAddress, Hash, TMAddress};
    use crate::PREFIX;
    use hex::ToHex;

    use super::PollStartedEvent;

    #[test]
    fn should_deserialize_poll_started_event() {
        let event: Result<PollStartedEvent, events::Error> = get_event(
            poll_started_event(participants(5, None)),
            &TMAddress::random(PREFIX),
        )
        .try_into();

        assert!(event.is_ok());

        let event = event.unwrap();

        assert!(event.poll_id == 100u64.into());
        assert!(
            event.source_gateway_address.to_bech32_string().unwrap()
                == "erd1qqqqqqqqqqqqqpgqsvzyz88e8v8j6x3wquatxuztnxjwnw92kkls6rdtzx"
        );

        let message = event.messages.get(0).unwrap();

        assert!(
            message.tx_id.encode_hex::<String>()
                == "dfaf64de66510723f2efbacd7ead3c4f8c856aed1afc2cb30254552aeda47312",
        );
        assert!(message.event_index == 1u32);
        assert!(message.destination_chain.to_string() == "ethereum");
        assert!(
            message.source_address.to_bech32_string().unwrap()
                == "erd1qqqqqqqqqqqqqpgqzqvm5ywqqf524efwrhr039tjs29w0qltkklsa05pk7"
        );
    }

    // Should not handle event if it is not a poll started event
    #[async_test]
    async fn not_poll_started_event() {
        let event = get_event(
            cosmwasm_std::Event::new("transfer"),
            &TMAddress::random(PREFIX),
        );

        let handler = super::Handler::new(
            TMAddress::random(PREFIX),
            TMAddress::random(PREFIX),
            MockMvxProxy::new(),
            watch::channel(0).1,
        );

        assert!(handler.handle(&event).await.is_ok());
    }

    // Should not handle event if it is not emitted from voting verifier
    #[async_test]
    async fn contract_is_not_voting_verifier() {
        let event = get_event(
            poll_started_event(participants(5, None)),
            &TMAddress::random(PREFIX),
        );

        let handler = super::Handler::new(
            TMAddress::random(PREFIX),
            TMAddress::random(PREFIX),
            MockMvxProxy::new(),
            watch::channel(0).1,
        );

        assert!(handler.handle(&event).await.is_ok());
    }

    // Should not handle event if worker is not a poll participant
    #[async_test]
    async fn verifier_is_not_a_participant() {
        let voting_verifier = TMAddress::random(PREFIX);
        let event = get_event(poll_started_event(participants(5, None)), &voting_verifier);

        let handler = super::Handler::new(
            TMAddress::random(PREFIX),
            voting_verifier,
            MockMvxProxy::new(),
            watch::channel(0).1,
        );

        assert!(handler.handle(&event).await.is_ok());
    }

    #[async_test]
    async fn failed_to_get_transactions_info_with_results() {
        let mut proxy = MockMvxProxy::new();
        proxy
            .expect_transactions_info_with_results()
            .returning(|_| Err(Report::from(Error::DeserializeEvent)));

        let voting_verifier = TMAddress::random(PREFIX);
        let worker = TMAddress::random(PREFIX);

        let event = get_event(
            poll_started_event(participants(5, Some(worker.clone()))),
            &voting_verifier,
        );

        let handler = super::Handler::new(worker, voting_verifier, proxy, watch::channel(0).1);

        assert!(matches!(
            *handler.handle(&event).await.unwrap_err().current_context(),
            Error::TxReceipts
        ));
    }

    #[async_test]
    async fn should_vote_correctly() {
        let mut proxy = MockMvxProxy::new();
        proxy
            .expect_transactions_info_with_results()
            .returning(|_| Ok(HashMap::new()));

        let voting_verifier = TMAddress::random(PREFIX);
        let worker = TMAddress::random(PREFIX);
        let event = get_event(
            poll_started_event(participants(5, Some(worker.clone()))),
            &voting_verifier,
        );

        let handler = super::Handler::new(worker, voting_verifier, proxy, watch::channel(0).1);

        let actual = handler.handle(&event).await.unwrap();
        assert_eq!(actual.len(), 1);
        assert!(MsgExecuteContract::from_any(actual.first().unwrap()).is_ok());
    }

    #[async_test]
    async fn should_skip_expired_poll() {
        let mut proxy = MockMvxProxy::new();
        proxy
            .expect_transactions_info_with_results()
            .returning(|_| Err(Report::from(Error::Finalizer)));

        let voting_verifier = TMAddress::random(PREFIX);
        let worker = TMAddress::random(PREFIX);
        let expiration = 100u64;
        let event = get_event(
            poll_started_event(participants(5, Some(worker.clone()))),
            &voting_verifier,
        );

        let (tx, rx) = watch::channel(expiration - 1);

        let handler = super::Handler::new(worker, voting_verifier, proxy, rx);

        // poll is not expired yet, should hit proxy error
        assert!(handler.handle(&event).await.is_err());

        let _ = tx.send(expiration + 1);

        // poll is expired, should not hit proxy error now
        assert_eq!(handler.handle(&event).await.unwrap(), vec![]);
    }

    fn poll_started_event(participants: Vec<TMAddress>) -> PollStarted {
        PollStarted::Messages {
            metadata: PollMetadata {
                poll_id: "100".parse().unwrap(),
                source_chain: "multiversx".parse().unwrap(),
                source_gateway_address:
                    "erd1qqqqqqqqqqqqqpgqsvzyz88e8v8j6x3wquatxuztnxjwnw92kkls6rdtzx"
                        .parse()
                        .unwrap(),
                confirmation_height: 15,
                expires_at: 100,
                participants: participants
                    .into_iter()
                    .map(|addr| cosmwasm_std::Addr::unchecked(addr.to_string()))
                    .collect(),
            },
            messages: vec![TxEventConfirmation {
                tx_id: "dfaf64de66510723f2efbacd7ead3c4f8c856aed1afc2cb30254552aeda47312"
                    .parse()
                    .unwrap(),
                event_index: 1,
                source_address: "erd1qqqqqqqqqqqqqpgqzqvm5ywqqf524efwrhr039tjs29w0qltkklsa05pk7"
                    .parse()
                    .unwrap(),
                destination_chain: "ethereum".parse().unwrap(),
                destination_address: format!("0x{:x}", EVMAddress::random()).parse().unwrap(),
                payload_hash: Hash::random().to_fixed_bytes(),
            }],
        }
    }

    fn participants(n: u8, worker: Option<TMAddress>) -> Vec<TMAddress> {
        (0..n)
            .into_iter()
            .map(|_| TMAddress::random(PREFIX))
            .chain(worker.into_iter())
            .collect()
    }
}
