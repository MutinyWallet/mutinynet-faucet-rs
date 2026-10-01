use bitcoin::{Address, Network};
use bitcoin_payment_instructions::hrn_resolution::DummyHrnResolver;
use bitcoin_payment_instructions::{
    PaymentInstructions, PaymentMethod, PossiblyResolvedPaymentMethod,
};
use lightning_invoice::Bolt11Invoice;

/// The payment methods used by the faucet after parsing a payment instruction.
pub(crate) struct ParsedPaymentInstructions {
    pub(crate) invoice: Option<Bolt11Invoice>,
    pub(crate) address: Option<Address>,
    pub(crate) onchain_sats: Option<u64>,
}

pub(crate) async fn parse_payment_instructions(
    instructions: &str,
    network: Network,
) -> anyhow::Result<ParsedPaymentInstructions> {
    let parsed = PaymentInstructions::parse(instructions, network, &DummyHrnResolver, false)
        .await
        .map_err(|error| anyhow::anyhow!("invalid payment instructions: {error:?}"))?;

    let mut invoice = None;
    let mut address = None;
    let onchain_sats = match &parsed {
        PaymentInstructions::FixedAmount(fixed) => {
            for method in fixed.methods() {
                collect_method(method, &mut invoice, &mut address);
            }
            fixed
                .onchain_payment_amount()
                .map(|amount| amount.sats_rounding_up())
        }
        PaymentInstructions::ConfigurableAmount(configurable) => {
            for method in configurable.methods() {
                if let PossiblyResolvedPaymentMethod::Resolved(method) = method {
                    collect_method(method, &mut invoice, &mut address);
                }
            }
            None
        }
    };

    Ok(ParsedPaymentInstructions {
        invoice,
        address,
        onchain_sats,
    })
}

fn collect_method(
    method: &PaymentMethod,
    invoice: &mut Option<Bolt11Invoice>,
    address: &mut Option<Address>,
) {
    match method {
        PaymentMethod::LightningBolt11(candidate) if invoice.is_none() => {
            *invoice = Some(candidate.clone());
        }
        PaymentMethod::OnChain(candidate) if address.is_none() => {
            *address = Some(candidate.clone());
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::parse_payment_instructions;
    use bitcoin::Network;

    const TESTNET_ADDRESS: &str = "tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx";

    #[tokio::test]
    async fn parses_raw_onchain_address() {
        let parsed = parse_payment_instructions(TESTNET_ADDRESS, Network::Testnet)
            .await
            .unwrap();
        assert_eq!(parsed.address.unwrap().to_string(), TESTNET_ADDRESS);
        assert_eq!(parsed.onchain_sats, None);
    }

    #[tokio::test]
    async fn parses_bip21_amount() {
        let parsed = parse_payment_instructions(
            &format!("bitcoin:{TESTNET_ADDRESS}?amount=0.00001000"),
            Network::Testnet,
        )
        .await
        .unwrap();
        assert_eq!(parsed.address.unwrap().to_string(), TESTNET_ADDRESS);
        assert_eq!(parsed.onchain_sats, Some(1_000));
    }

    #[tokio::test]
    async fn rejects_wrong_network() {
        assert!(
            parse_payment_instructions(TESTNET_ADDRESS, Network::Bitcoin)
                .await
                .is_err()
        );
    }
}
