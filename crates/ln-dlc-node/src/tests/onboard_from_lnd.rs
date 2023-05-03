use crate::node::Node;
use crate::tests::init_tracing;
use crate::tests::lnd::LndNode;
use crate::tests::log_channel_id;
use bitcoin::Amount;
use std::time::Duration;

#[tokio::test]
#[ignore]
async fn onboard_from_lnd() {
    init_tracing();

    let coordinator = Node::start_test_coordinator("coordinator").await.unwrap();
    let payee = Node::start_test_app("payee").await.unwrap();
    let payer = LndNode::new();

    payee.connect(coordinator.info).await.unwrap();

    // Fund the on-chain wallets of the nodes who will open a channel
    coordinator.fund(Amount::from_sat(1_000_000)).await.unwrap();
    payer.fund(Amount::from_sat(1_000_000)).await.unwrap();

    payer
        .open_channel(&coordinator, Amount::from_sat(50_000))
        .await
        .unwrap();

    log_channel_id(&coordinator, 0, "lnd-coordinator");

    coordinator.sync().unwrap();
    payee.sync().unwrap();

    // The coordinator must send a `NodeAnnouncement` to LND before LND sends the payment, as
    // otherwise we will encounter an error in the coordinator when processing the incoming HTLC:
    // `Unable to decode our hop data` because of a `DecodeError::InvalidValue`.
    coordinator.broadcast_node_announcement();
    tokio::time::sleep(Duration::from_secs(2)).await;

    let invoice_amount = 1000;

    let intercepted_scid_details = coordinator.create_intercept_scid(payee.info.pubkey);
    let fake_scid = intercepted_scid_details.scid;
    let fee_millionth = intercepted_scid_details.jit_routing_fee_millionth;
    let invoice = payee
        .create_interceptable_invoice(
            Some(invoice_amount),
            fake_scid,
            coordinator.info.pubkey,
            0,
            "".to_string(),
            fee_millionth,
        )
        .unwrap();

    // lnd sends the payment
    payer.send_payment(invoice).await.unwrap();

    // For the payment to be claimed before the wallets sync
    tokio::time::sleep(Duration::from_secs(3)).await;

    coordinator.sync().unwrap();
    payee.sync().unwrap();

    // Assert

    let payee_balance = payee.get_ldk_balance();
    assert_eq!(invoice_amount, payee_balance.available);
}
