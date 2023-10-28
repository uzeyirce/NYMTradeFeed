use crate::{
    mongodb_client_subscan::MongoDbClientSubscan,
    mongodb_client_validator::MongoDbClientValidator,
    subscan_parser::{Network, SubscanParser},
    ExtrinsicsType, Module, SubscanOperation, Validator,
};
use futures::{stream::FuturesUnordered, StreamExt};
use itertools::Itertools;
use rs_exchanges_parser::{
    mongodb_client_exchanges::MongoDbClientExchanges, PrimaryToken, SecondaryToken,
};
use sp_core::crypto::{AccountId32, Ss58AddressFormat, Ss58Codec};
use std::env;
use strum::IntoEnumIterator;

pub async fn parse_staking() -> Option<Vec<SubscanOperation>> {
    let price_task = tokio::spawn(async move {
        let mut mongodb_client_exchanges = MongoDbClientExchanges::new().await;
        mongodb_client_exchanges
            .get_usd_price(PrimaryToken::Azero, SecondaryToken::Usdt)
            .await
    });

    let mut tasks = FuturesUnordered::new();
    for e in ExtrinsicsType::iter() {
        tasks.push(tokio::spawn(async move {
            let subscan_api_key = &env::var("SUBSCAN_API_KEY").unwrap();
            let mut subscan_parser = SubscanParser::new(Network::Alephzero, subscan_api_key).await;
            subscan_parser
                .parse_subscan_operations("", Module::Staking, e, 10)
                .await
        }));
    }

    let mut subscan_operations = Vec::new();
    while let Some(res) = tasks.next().await {
        let Ok(s) = res else {
            continue;
        };

        let Some(mut s) = s else {
            continue;
        };
        subscan_operations.append(&mut s);
    }

    // skipping already existing records
    let mut mongodb_client_subscan = MongoDbClientSubscan::new().await;
    let subscan_operations = mongodb_client_subscan
        .get_not_existing_operations(subscan_operations)
        .await;

    // adding to_wallet and operation_quantity
    let mut tasks = FuturesUnordered::new();
    for s in subscan_operations {
        let mut s_clone = s.clone();
        tasks.push(tokio::spawn(async move {
            let subscan_api_key = &env::var("SUBSCAN_API_KEY").unwrap();
            let mut subscan_parser = SubscanParser::new(Network::Alephzero, subscan_api_key).await;
            let events = subscan_parser
                .parse_subscan_extrinsic_details(s.extrinsic_index)
                .await?;

            let stake_event = events.get(1)?;

            // event must have at least 2 parameters
            if stake_event.event_params.len() < 2 {
                return None;
            }

            let stash_param = stake_event.event_params.first()?;
            if stash_param.name != "stash" && stash_param.name != "who" {
                return None;
            }

            let amount_param = stake_event.event_params.last()?;
            if amount_param.name != "amount" {
                return None;
            }

            let stash_wallet = stash_param.value.clone()[2..].to_string();
            let decoded = hex::decode(stash_wallet).ok()?;
            let byte_arr: [u8; 32] = decoded.try_into().ok()?;
            let address = AccountId32::from(byte_arr)
                .to_ss58check_with_version(Ss58AddressFormat::custom(42));
            s_clone.from_wallet = address;
            s_clone.to_wallet = "0x0".to_string();
            s_clone.operation_quantity = amount_param.value.parse::<f64>().ok()? / 1e12;

            Some(s_clone)
        }));
    }

    let mut subscan_operations = Vec::new();
    while let Some(res) = tasks.next().await {
        let Ok(s) = res else {
            continue;
        };

        let Some(s) = s else {
            continue;
        };
        subscan_operations.push(s);
    }

    // parsing batch all operations
    let batch_all_operations = tokio::spawn(async move {
        let subscan_api_key = &env::var("SUBSCAN_API_KEY").unwrap();
        let mut subscan_parser = SubscanParser::new(Network::Alephzero, subscan_api_key).await;
        subscan_parser.parse_subscan_batch_all("", 0, 10).await
    })
    .await
    .ok()??;

    // saving validators to db
    let validators = convert_operations_to_validators(subscan_operations.clone());
    let validators_task = tokio::spawn(async move {
        let mut mongodb_client_validator = MongoDbClientValidator::new().await;
        mongodb_client_validator
            .import_or_update_validators(validators)
            .await
    });

    // skipping already existing records
    let mut batch_all_operations = mongodb_client_subscan
        .get_not_existing_operations(batch_all_operations)
        .await;

    subscan_operations.append(&mut batch_all_operations);

    // updating to current price
    let price = price_task.await.ok()??;
    for s in subscan_operations.iter_mut() {
        s.operation_usd = s.operation_quantity * price;
    }

    validators_task.await.ok()?;

    // getting nominators missing in validators DB to update them
    let nominators = subscan_operations
        .iter()
        .map(|m| m.from_wallet.clone())
        .unique()
        .collect::<Vec<String>>();
    let mut mongodb_client_validator = MongoDbClientValidator::new().await;
    let not_existing_nominators = mongodb_client_validator
        .get_not_existing_nominators(nominators)
        .await;

    // parsing validators for given non existing nominators
    let mut tasks = FuturesUnordered::new();
    for nominator in not_existing_nominators.into_iter() {
        let nominator_clone = nominator.clone();
        tasks.push(tokio::spawn(async move {
            let subscan_api_key = &env::var("SUBSCAN_API_KEY").unwrap();
            let mut subscan_parser = SubscanParser::new(Network::Alephzero, subscan_api_key).await;
            subscan_parser
                .parse_subscan_batch_all(&nominator_clone, 0, 1)
                .await
        }));

        tasks.push(tokio::spawn(async move {
            let subscan_api_key = &env::var("SUBSCAN_API_KEY").unwrap();
            let mut subscan_parser = SubscanParser::new(Network::Alephzero, subscan_api_key).await;
            subscan_parser
                .parse_subscan_operations(&nominator, Module::Staking, ExtrinsicsType::Nominate, 1)
                .await
        }));
    }

    let mut validators = Vec::new();
    while let Some(res) = tasks.next().await {
        let Ok(s) = res else {
            continue;
        };

        let Some(s) = s else {
            continue;
        };

        let mut v = convert_operations_to_validators(s);
        validators.append(&mut v);
    }

    // updating validators
    mongodb_client_validator
        .import_or_update_validators(validators)
        .await;

    for s in subscan_operations.iter_mut() {
        let to_wallet = mongodb_client_validator
            .get_validator_by_nominator(&s.from_wallet)
            .await;
        let Some(to_wallet) = to_wallet else {
            s.set_hash();
            continue;
        };
        s.to_wallet = to_wallet.validator;
        s.set_hash();
    }

    Some(subscan_operations)
}

fn convert_operations_to_validators(source: Vec<SubscanOperation>) -> Vec<Validator> {
    source
        .into_iter()
        .filter_map(|p| {
            if p.to_wallet == "0x0" {
                return None;
            }

            Some(Validator {
                nominator: p.from_wallet,
                validator: p.to_wallet,
            })
        })
        .collect()
}
