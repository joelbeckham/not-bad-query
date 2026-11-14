# AWS public CA certificates

`aws-rds-global-bundle.pem` is the unmodified public commercial-region bundle from
https://truststore.pki.rds.amazonaws.com/global/global-bundle.pem on 2026-11-12.

SHA-256: `90a6f1a7978a1b1b096dca6c57511ca48d3b8cf052f8e704baf358f34ea2f21b`

In Verify full mode these roots supplement OS trust for `.rds.amazonaws.com` and
`.redshift.amazonaws.com` hosts when no custom CA is supplied. Hostname, validity,
and certificate-chain checks remain enabled. Custom aliases and other regions
can import their provider's public PEM bundle. Never store private keys here.
