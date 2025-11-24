cargo run -r --bin sliquidator -- --log-console \
--base-ipc /tmp/anvil.ipc \
--base-key-file base_key \
--soldier-mnemonic-file soldier_mnemonic.txt \
--soldiers 20 \
--soldier-min-wei 1000000000000000000 \
--soldier-target-wei 5000000000000000000 \
--endpoint /tmp/anvil.ipc \
--endpoint https://rpc.soniclabs.com \
