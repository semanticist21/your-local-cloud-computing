[English](../gpu.md) | 한국어

# GPU와 플랫폼 확인

프로그램이 요청한 GPU 백엔드를 실제로 사용해야 합니다. `--gpu cuda`, `--gpu rocm`, `--gpu metal`은 서로 다른 기능을 선택하며, CPU 코드를 변환하거나 네트워크 너머로 GPU 메모리를 합쳐 주지 않습니다. `workers`를 확인하고 실제 워크로드를 실행해, 서비스 계정에서 장치/드라이버 접근이 되는지 검증하세요. 컨테이너에서 GPU를 쓰려면 추가로 호스트의 NVIDIA/CDI 또는 ROCm 런타임 설정이 필요합니다.

[지원 체크아웃](install.md#예제와-에이전트-지원-파일-내려받기)에서, [CUDA 드라이버 예제](../../examples/jobs/cuda.py)는 Python 표준 라이브러리와 설치된 NVIDIA 드라이버로 GPU 벡터 덧셈을 수행합니다. [Metal 예제](../../examples/jobs/metal.swift)는 Metal 컴퓨트 커널을 디스패치합니다.

```bash
(
  cd "$YLCC_SUPPORT_DIR"
  ylcc run --input examples/jobs/cuda.py --os linux --arch x86_64 --gpu cuda \
    --output result.json -- python3 examples/jobs/cuda.py
  ylcc run --input examples/jobs/metal.swift --os macos --arch aarch64 --gpu metal \
    --output result.json -- swift examples/jobs/metal.swift
)
```

macOS/Xcode 작업에는 네이티브 Mac 워커와 적절한 SDK가 필요합니다. Apple Silicon 노트북에서 Linux x86_64를 요청하면 조건에 맞는 워커가 선택될 뿐, 결과물이 macOS 실행 파일이 되지는 않습니다. 크로스 컴파일에는 관련 링커/sysroot/SDK와 명시적인 빌드 명령이 필요합니다. v0.2.0에는 에뮬레이션이 구현되어 있지 않습니다.
