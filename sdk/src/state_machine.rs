use std::collections::BTreeMap;

use cosmwasm_std::{Binary, Coin, ContractResult, Empty, Event, Response};
use cosmwasm_vm::testing::{mock_env, mock_info};
use cosmwasm_vm::{
    call_execute, call_instantiate, call_query, Backend, Instance, InstanceOptions, Storage,
};
use thiserror::Error;

use crate::hash::sha256;
use crate::msg::{
    Account, AccountResponse, Code, CodeResponse, Contract, ContractResponse, GenesisState, SdkMsg,
    SdkQuery, Tx, WasmRawResponse, WasmSmartResponse,
};
use crate::store::ContractStore;
use crate::{auth, wasm};

/// The application's state and state transition rules. The core of the blockchain.
#[derive(Debug, Default)]
pub struct State {
    /// Current block height
    pub height: u64,

    /// Identifier of the chain
    pub chain_id: String,
    /// The total number of wasm byte codes stored
    pub code_count: u64,
    /// The total number of contracts instantiated
    pub contract_count: u64,

    /// User accounts: Address -> Account
    /// TODO: use &str instead of String as key?
    pub accounts: BTreeMap<String, Account>,

    /// Wasm byte codes indexed by the ids
    pub codes: BTreeMap<u64, Code>,

    /// The code id used by each contract
    pub contracts: BTreeMap<u64, Contract>,

    /// Contract store
    pub stores: BTreeMap<u64, ContractStore>,
}

// public functions for the state machine
impl State {
    /// Returns ABCI info response.
    ///
    /// For now, our mock storage doesn't provide a method to generate the app hash. Instead, we
    /// simply return `sha256(height)` as a mock app hash.
    pub fn info(&self) -> (u64, Vec<u8>) {
        let app_hash = sha256(&self.height.to_be_bytes());
        (self.height, app_hash)
    }

    /// Run genesis messages. Return app hash.
    /// TODO: Once a staking contract is created, return the genesis validator set as well.
    pub fn init_chain(&mut self, app_state_bytes: &[u8]) -> Result<Vec<u8>, StateError> {
        let GenesisState {
            deployer,
            gen_msgs,
        } = serde_json::from_slice(app_state_bytes)?;

        // TODO: validate deployer address

        for msg in gen_msgs {
            match msg {
                SdkMsg::StoreCode {
                    wasm_byte_code,
                } => {
                    self.store_code(&deployer, wasm_byte_code)?;
                },
                SdkMsg::Instantiate {
                    code_id,
                    msg,
                    funds,
                    label,
                    admin,
                } => {
                    self.instantiate_contract(&deployer, code_id, msg.into(), funds, label, admin)?;
                },
                SdkMsg::Execute {
                    contract,
                    msg,
                    funds,
                } => {
                    self.execute_contract(&deployer, contract, msg.into(), funds)?;
                },
                SdkMsg::Migrate {
                    contract,
                    code_id,
                    msg,
                } => {
                    self.migrate_contract(&deployer, contract, code_id, msg.into())?;
                },
            }
        }

        let (_, app_hash) = self.info();
        Ok(app_hash)
    }

    /// Handle ABCI queries. Return query responses as raw binaries.
    pub fn handle_query(&self, query_bytes: &[u8]) -> Result<Vec<u8>, StateError> {
        // deserialize the query from bytes
        let query: SdkQuery = serde_json::from_slice(query_bytes)?;

        match query {
            SdkQuery::Account {
                address,
            } => serde_json::to_vec(&self.query_account(&address)?),
            SdkQuery::Code {
                code_id,
            } => serde_json::to_vec(&self.query_code(code_id)?),
            SdkQuery::Contract {
                contract,
            } => serde_json::to_vec(&self.query_contract(contract)?),
            SdkQuery::WasmRaw {
                contract,
                key,
            } => serde_json::to_vec(&self.query_wasm_raw(contract, key.as_slice())?),
            SdkQuery::WasmSmart {
                contract,
                msg,
            } => serde_json::to_vec(&self.query_wasm_smart(contract, msg.as_slice())?),
        }
        .map_err(StateError::from)
    }

    /// Handle transactions. Returns events emitted during transaction executions.
    pub fn handle_tx(&mut self, tx_bytes: &[u8]) -> Result<Vec<Event>, StateError> {
        // deserialize the tx from bytes
        let tx: Tx = serde_json::from_slice(tx_bytes)?;

        // authenticate signature, chain id, sequence, etc.
        let account = auth::authenticate_tx(&tx, self)?;

        // increment the sender's sequence number
        self.accounts.insert(tx.body.sender.clone(), account);

        let mut events = vec![];

        tx.body
            .msgs
            .into_iter()
            .map(|msg| match msg {
                SdkMsg::StoreCode {
                    wasm_byte_code,
                } => {
                    let event = self.store_code(&tx.body.sender, wasm_byte_code)?;
                    Ok(vec![event])
                },
                SdkMsg::Instantiate {
                    code_id,
                    msg,
                    funds,
                    label,
                    admin,
                } => self.instantiate_contract(&tx.body.sender, code_id, msg.into(), funds, label, admin),
                SdkMsg::Execute {
                    contract,
                    msg,
                    funds,
                } => self.execute_contract(&tx.body.sender, contract, msg.into(), funds),
                SdkMsg::Migrate {
                    contract,
                    code_id,
                    msg,
                } => self.migrate_contract(&tx.body.sender, contract, code_id, msg.into()),
            })
            .try_for_each(|res| -> Result<_, StateError> {
                events.extend(res?);
                Ok(())
            })?;

        Ok(events)
    }

    /// Commit changes in the cached state into the main application state, and advance block
    /// height by 1. Return the updated block height and app hash.
    ///
    /// TODO: Ideally the state machine maintains a cached state for uncommitted changes separate
    /// from the "main" state, and only commits changes in the cached state into the main state upon
    /// this function call. However for now we don't have such a mechanism implemented.
    pub fn commit(&mut self) -> (u64, Vec<u8>) {
        self.height += 1;
        self.info()
    }
}

// private functions for the state machine
impl State {
    fn store_code(
        &mut self,
        sender: &str,
        wasm_byte_code: Binary,
    ) -> Result<Event, StateError> {
        let hash = sha256(wasm_byte_code.as_slice());
        let hash_str = hex::encode(&hash);

        // increment code count
        self.code_count += 1;

        // insert code into the map
        let code_id = self.code_count;
        self.codes.insert(
            code_id,
            Code {
                creator: sender.into(),
                wasm_byte_code,
            },
        );

        Ok(Event::new("store_code")
            .add_attribute("code_id", code_id.to_string())
            .add_attribute("sender", sender)
            .add_attribute("hash", hash_str))
    }

    /// TODO: need to check there is no collision between the contract address and account address
    /// before committing the newly instantiated contract to the store
    fn instantiate_contract(
        &mut self,
        sender: &str,
        code_id: u64,
        msg: Vec<u8>,
        funds: Vec<Coin>,
        label: String,
        admin: Option<String>,
    ) -> Result<Vec<Event>, StateError> {
        if !funds.is_empty() {
            return Err(StateError::FundsUnsupported);
        }

        let backend = wasm::create_backend(ContractStore::new());
        let code = &self.codes[&code_id];
        let mut instance = Instance::from_code(
            &code.wasm_byte_code,
            backend,
            InstanceOptions {
                gas_limit: u64::MAX,
                print_debug: true,
            },
            None,
        )?;
        let result: ContractResult<Response<Empty>> = call_instantiate(
            &mut instance,
            &mock_env(),
            &mock_info(sender, &[]),
            &msg,
        )?;

        let Backend {
            storage,
            ..
        } = instance.recycle().unwrap();

        match result {
            ContractResult::Ok(response) => {
                if !response.messages.is_empty() {
                    return Err(StateError::SubmessagesUnsupported);
                }

                // increment contract count
                self.contract_count += 1;

                // for now, we just use a number as contract address
                let contract_addr = self.contract_count;
                self.contracts.insert(
                    contract_addr,
                    Contract {
                        code_id,
                        label,
                        admin,
                    },
                );
                self.stores.insert(contract_addr, storage);

                // collect the events
                let event = Event::new("instantiate_contract")
                    .add_attribute("sender", sender)
                    .add_attribute("code_id", code_id.to_string())
                    .add_attribute("contract_address", contract_addr.to_string())
                    .add_attributes(response.attributes);

                Ok(prepend(event, response.events))
            },
            ContractResult::Err(err) => Err(StateError::Contract(err)),
        }
    }

    fn execute_contract(
        &mut self,
        sender: &str,
        contract_addr: u64,
        msg: Vec<u8>,
        funds: Vec<Coin>,
    ) -> Result<Vec<Event>, StateError> {
        if !funds.is_empty() {
            return Err(StateError::FundsUnsupported);
        }

        let storage = self
            .stores
            .get(&contract_addr)
            .ok_or_else(|| StateError::contract_not_found(contract_addr))?
            .clone();
        let contract = &self.contracts[&contract_addr];
        let code = &self.codes[&contract.code_id];
        let backend = wasm::create_backend(storage);
        let mut instance = Instance::from_code(
            &code.wasm_byte_code,
            backend,
            InstanceOptions {
                gas_limit: u64::MAX,
                print_debug: true,
            },
            None,
        )?;
        let result: ContractResult<Response<Empty>> = call_execute(
            &mut instance,
            &mock_env(),
            &mock_info(sender, &[]),
            &msg,
        )?;

        let Backend {
            storage,
            ..
        } = instance.recycle().unwrap();

        match result {
            ContractResult::Ok(response) => {
                if !response.messages.is_empty() {
                    return Err(StateError::SubmessagesUnsupported);
                }

                self.stores.insert(contract_addr, storage);

                // collect the events
                let event = Event::new("execute_contract")
                    .add_attribute("sender", sender)
                    .add_attribute("contract_address", contract_addr.to_string())
                    .add_attributes(response.attributes);

                Ok(prepend(event, response.events))
            },
            ContractResult::Err(err) => Err(StateError::Contract(err)),
        }
    }

    fn migrate_contract(
        &self,
        _sender: &str,
        _contract_addr: u64,
        _code_id: u64,
        _msg: Vec<u8>,
    ) -> Result<Vec<Event>, StateError> {
        Err(StateError::MigrationUnsupported)
    }

    fn query_account(&self, address: &str) -> Result<AccountResponse, StateError> {
        match self.accounts.get(address) {
            Some(account) => Ok(AccountResponse {
                address: address.into(),
                pubkey: Some(account.pubkey.clone()),
                sequence: account.sequence,
            }),
            None => Ok(AccountResponse {
                address: address.into(),
                pubkey: None,
                sequence: 0,
            }),
        }
    }

    fn query_code(&self, code_id: u64) -> Result<CodeResponse, StateError> {
        match self.codes.get(&code_id) {
            Some(code) => Ok(code.clone().into()),
            None => Err(StateError::code_not_found(code_id)),
        }
    }

    fn query_contract(&self, contract_addr: u64) -> Result<ContractResponse, StateError> {
        self.contracts
            .get(&contract_addr)
            .cloned()
            .ok_or_else(|| StateError::contract_not_found(contract_addr))
    }

    fn query_wasm_raw(&self, contract_addr: u64, key: &[u8]) -> Result<WasmRawResponse, StateError> {
        let storage = self
            .stores
            .get(&contract_addr)
            .cloned()
            .ok_or_else(|| StateError::contract_not_found(contract_addr))?;
        let (res, _) = storage.get(key);
        let value = res?;
        Ok(WasmRawResponse {
            contract: contract_addr,
            key: key.to_owned().into(),
            value: value.map(Binary),
        })
    }

    fn query_wasm_smart(&self, contract_addr: u64, msg: &[u8]) -> Result<WasmSmartResponse, StateError> {
        let storage = self
            .stores
            .get(&contract_addr)
            .cloned()
            .ok_or_else(|| StateError::contract_not_found(contract_addr))?;
        let contract = &self.contracts[&contract_addr];
        let code = &self.codes[&contract.code_id];
        let backend = wasm::create_backend(storage);
        let mut instance = Instance::from_code(
            &code.wasm_byte_code,
            backend,
            InstanceOptions {
                gas_limit: u64::MAX,
                print_debug: true,
            },
            None,
        )?;
        let result = call_query(&mut instance, &mock_env(), msg)?;
        Ok(WasmSmartResponse {
            contract: contract_addr,
            result,
        })
    }
}

#[derive(Debug, Error)]
pub enum StateError {
    #[error(transparent)]
    Backend(#[from] cosmwasm_vm::BackendError),

    #[error(transparent)]
    Vm(#[from] cosmwasm_vm::VmError),

    #[error(transparent)]
    Serde(#[from] serde_json::Error),

    #[error(transparent)]
    Auth(#[from] auth::AuthError),

    #[error("contract emitted error: {0}")]
    Contract(String),

    #[error("no wasm binary code found with the id {code_id}")]
    CodeNotFound {
        code_id: u64,
    },

    #[error("no contract found under the address {address}")]
    ContractNotFound {
        address: u64,
    },

    #[error("contract response includes submessages, which is not supported yet")]
    SubmessagesUnsupported,

    #[error("sending funds when instantiating or executing contracts is not supported yet")]
    FundsUnsupported,

    #[error("migrating contracts is not supported yet")]
    MigrationUnsupported,
}

impl StateError {
    pub fn code_not_found(code_id: u64) -> Self {
        Self::CodeNotFound {
            code_id,
        }
    }

    pub fn contract_not_found(address: u64) -> Self {
        Self::ContractNotFound {
            address,
        }
    }
}

/// Insert an event to the front of an array of events.
/// https://www.reddit.com/r/rust/comments/kul4qz/vec_prepend_insert_from_slice/
fn prepend(event: Event, mut events: Vec<Event>) -> Vec<Event> {
    events.splice(..0, vec![event]);
    events
}
