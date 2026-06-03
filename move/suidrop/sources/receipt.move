/// SuiDrop on-chain receipts.
///
/// Each time a file is dropped, the sender mints a `DropReceipt` object that
/// anchors the Walrus blob id and drop metadata on Sui. The receiver can then
/// verify the drop trustlessly by reading the object via RPC (Tatum).
///
/// Privacy: only a SHA-256 *hash* of the filename is stored on-chain, never the
/// plaintext name. The decryption key never touches the chain — it lives in the
/// share-link fragment only.
module suidrop::receipt {
    use std::string::String;
    use sui::clock::Clock;
    use sui::event;

    /// On-chain proof that a file was dropped via SuiDrop.
    public struct DropReceipt has key, store {
        id: UID,
        /// Walrus blob id (base64url string) of the encrypted blob.
        blob_id: String,
        /// Who created the drop.
        sender: address,
        /// Intended recipient, or @0x0 for "anyone with the link".
        recipient: address,
        /// Size in bytes of the encrypted blob.
        size: u64,
        /// SHA-256 hash of the original filename (no plaintext on-chain).
        name_hash: vector<u8>,
        /// Creation time in epoch milliseconds.
        created_at_ms: u64,
        /// Walrus storage duration the blob was paid for, in epochs.
        expiry_epochs: u64,
    }

    /// Emitted on every drop so indexers can track activity.
    public struct DropCreated has copy, drop {
        receipt_id: ID,
        blob_id: String,
        sender: address,
        recipient: address,
        created_at_ms: u64,
    }

    /// Mint a receipt for a drop and transfer ownership to the sender.
    entry fun create_receipt(
        blob_id: String,
        recipient: address,
        size: u64,
        name_hash: vector<u8>,
        expiry_epochs: u64,
        clock: &Clock,
        ctx: &mut TxContext,
    ) {
        let sender = ctx.sender();
        let now = clock.timestamp_ms();

        let receipt = DropReceipt {
            id: object::new(ctx),
            blob_id,
            sender,
            recipient,
            size,
            name_hash,
            created_at_ms: now,
            expiry_epochs,
        };

        event::emit(DropCreated {
            receipt_id: object::id(&receipt),
            blob_id: receipt.blob_id,
            sender,
            recipient,
            created_at_ms: now,
        });

        transfer::public_transfer(receipt, sender);
    }

    #[test_only]
    use sui::test_scenario as ts;
    #[test_only]
    use sui::clock;
    #[test_only]
    use std::string;

    /// Mint a receipt and assert the sender ends up owning it with the exact
    /// fields we passed in (including the clock timestamp).
    #[test]
    fun creates_receipt_owned_by_sender() {
        let sender = @0xA11CE;
        let mut scenario = ts::begin(sender);

        // tx 1: create the receipt
        {
            let mut clock = clock::create_for_testing(ts::ctx(&mut scenario));
            clock::set_for_testing(&mut clock, 1000);
            create_receipt(
                string::utf8(b"BLOB_ABC"),
                @0x0, // open drop: anyone with the link
                42,
                b"hash-bytes",
                5,
                &clock,
                ts::ctx(&mut scenario),
            );
            clock::destroy_for_testing(clock);
        };

        // tx 2: the receipt should now be owned by the sender
        ts::next_tx(&mut scenario, sender);
        {
            let receipt = ts::take_from_sender<DropReceipt>(&scenario);
            assert!(receipt.blob_id == string::utf8(b"BLOB_ABC"), 0);
            assert!(receipt.sender == sender, 1);
            assert!(receipt.recipient == @0x0, 2);
            assert!(receipt.size == 42, 3);
            assert!(receipt.name_hash == b"hash-bytes", 4);
            assert!(receipt.created_at_ms == 1000, 5);
            assert!(receipt.expiry_epochs == 5, 6);
            ts::return_to_sender(&scenario, receipt);
        };

        ts::end(scenario);
    }

    /// A drop locked to a specific recipient records that address.
    #[test]
    fun records_named_recipient() {
        let sender = @0xA11CE;
        let recipient = @0xB0B;
        let mut scenario = ts::begin(sender);
        {
            let clock = clock::create_for_testing(ts::ctx(&mut scenario));
            create_receipt(
                string::utf8(b"BLOB_XYZ"),
                recipient,
                7,
                b"h",
                3,
                &clock,
                ts::ctx(&mut scenario),
            );
            clock::destroy_for_testing(clock);
        };
        ts::next_tx(&mut scenario, sender);
        {
            let receipt = ts::take_from_sender<DropReceipt>(&scenario);
            assert!(receipt.recipient == recipient, 0);
            ts::return_to_sender(&scenario, receipt);
        };
        ts::end(scenario);
    }
}
