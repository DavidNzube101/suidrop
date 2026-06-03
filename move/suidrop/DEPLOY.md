# Deploying the SuiDrop Move package

This mints the `DropReceipt` objects that anchor each drop on Sui.

## 1. Install the Sui CLI (once)

```bash
# Rust toolchain required. Then:
cargo install --locked --git https://github.com/MystenLabs/sui.git --branch testnet sui
sui --version
```

(Or use a prebuilt binary from the Sui releases page if you prefer.)

## 2. Point the CLI at testnet and fund your address

```bash
sui client new-env --alias testnet --rpc https://sui-testnet.gateway.tatum.io --header x-api-key:$TATUM_API_KEY
sui client switch --env testnet
sui client active-address          # your address
sui client faucet                  # get testnet SUI for gas
sui client gas                     # confirm you have coins
```

> Tip: this routes your CLI through **Tatum's** RPC too — same key as the app.

## 3. Publish

```bash
cd move/suidrop
sui client publish --gas-budget 100000000
```

Copy the **packageID** from the output (look under "Published Objects" /
`packageId`). Then put it in your `.env`:

```
SUIDROP_PACKAGE_ID=0x<your-package-id>
```

Restart the backend. The "Anchor on Sui with Nightly" button now lights up.

## 4. Going to mainnet for the demo

Switch `SUIDROP_NETWORK=mainnet`, re-publish the package against a mainnet env
(you'll need real SUI for gas), and set the new `SUIDROP_PACKAGE_ID`. Everything
else is identical.
