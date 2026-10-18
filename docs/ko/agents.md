[English](../agents.md) | 한국어

# 에이전트에서 사용

CLI를 설치하고 요청자를 설정한 뒤 [지원 파일을 내려받고](install.md#예제와-에이전트-지원-파일-내려받기), 함께 제공되는 스킬을 전역 또는 프로젝트별로 설치하세요.

```bash
bash "$YLCC_SUPPORT_DIR/scripts/install-skill.sh" --global
# Or:
bash "$YLCC_SUPPORT_DIR/scripts/install-skill.sh" --project /path/to/project
```

새 에이전트 세션을 시작하세요. Codex는 `~/.agents/skills`나 프로젝트의 `.agents/skills`에서 스킬을 찾으며, 포함된 스킬은 일반적인 자동 선택을 허용합니다. [Codex 스킬](https://learn.chatgpt.com/docs/customization/overview#skills)을 참고하세요. 오픈 스킬 형식을 지원하는 다른 에이전트는 문서화된 스킬 디렉터리에서 [skills/ylcc/SKILL.md](../../skills/ylcc/SKILL.md)를 불러올 수 있습니다.

자연스럽게 요청하세요. “이거 Linux x86_64 워커에서 빌드해서 바이너리 가져와 줘”처럼 말하거나, 명시적으로 `$ylcc`를 호출하면 됩니다. 스킬은 워커를 찾고, 요구 조건과 선택 입력을 정하고, 비동기로 제출하고, 로그/상태를 확인하고, 선언된 결과를 내려받습니다. MCP 서버를 추가하거나 워커에서 다른 에이전트를 실행하지 않습니다.

스킬을 설치한 뒤 이런 자연어 프롬프트를 시도해 보세요.
- "풀과 워커 상태만 확인하고, 아무것도 실행하지 마."
- "전원이 켜진 Windows x86_64 워커에서만 실행해."
- "이거 Linux x86_64에서 빌드하고 바이너리 가져와."
- "검토한 `ylcc.yaml`의 build 태스크를 쓰고, 끝날 때까지 기다렸다가 결과를 가져와."
- "채팅에 토큰을 보여 주지 말고 대시보드를 열어 줘."
