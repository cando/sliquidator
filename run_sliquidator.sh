cargo run -r --bin sliquidator -- --log-console \
--base-ipc /tmp/anvil.ipc \
--base-key-file base_key \
--eoa-key base_key \
--eoa-key base_key_2 \
--eoa-key base_key_3 \
--endpoint /tmp/anvil.ipc \
--endpoint https://rpc.soniclabs.com \
--endpoint https://rpc.soniclabs.com
