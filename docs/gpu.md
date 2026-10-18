English | [한국어](ko/gpu.md)

# GPU and platform checks

Programs must actually use the requested GPU backend. `--gpu cuda`, `--gpu rocm` and `--gpu metal` select distinct capabilities; they do not translate CPU code or pool GPU memory over the network. Check `workers` and run an actual workload to validate device/driver access under the service account. Container GPU execution additionally needs the host's NVIDIA/CDI or ROCm runtime configuration.

From the [support checkout](install.md#download-examples-and-agent-support-files), the [CUDA driver example](../examples/jobs/cuda.py) performs GPU vector addition using Python's standard library and an installed NVIDIA driver. The [Metal example](../examples/jobs/metal.swift) dispatches a Metal compute kernel:

```bash
(
  cd "$YLCC_SUPPORT_DIR"
  ylcc run --input examples/jobs/cuda.py --os linux --arch x86_64 --gpu cuda \
    --output result.json -- python3 examples/jobs/cuda.py
  ylcc run --input examples/jobs/metal.swift --os macos --arch aarch64 --gpu metal \
    --output result.json -- swift examples/jobs/metal.swift
)
```

macOS/Xcode work requires a native Mac worker and the appropriate SDK. Requesting Linux x86_64 from an Apple Silicon laptop selects a matching worker; it does not make the result a macOS executable. Cross-compilation needs the relevant linker/sysroot/SDK and an explicit build command. Emulation is not implemented.
