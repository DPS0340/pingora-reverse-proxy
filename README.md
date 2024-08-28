# pingora-reverse-proxy

[![Artifact Hub](https://img.shields.io/endpoint?url=https://artifacthub.io/badge/repository/pingora-reverse-proxy)](https://artifacthub.io/packages/search?repo=pingora-reverse-proxy)

> Dynamic Reverse Proxy using pingora & redis-rs & axum

## Run

```bash
RUST_LOG=debug cargo run
```

## TODOs

- [ ] (Thread Safe) HashMap based ctx
- [ ] Handle ws & wss
- [ ] Implement mgmt api
- [x] Implement Image build actions (CD)
- [ ] Implement helm chart & Integrate cluster develop tool e.g. devspace
- [ ] Implement Decompress -> Modify -> Compress Response body
