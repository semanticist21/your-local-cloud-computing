# Renders assets/demo.gif against a real pool: `vhs assets/demo.tape` from the repo root.
# Needs a configured requester and an online Windows x86_64 worker with rustc.
Output assets/demo.gif
Set Shell bash
Set FontSize 18
Set Width 1200
Set Height 440
Set Padding 24
Set TypingSpeed 35ms
Set Theme "Catppuccin Mocha"

Hide
Type "export PS1='$ ' && clear"
Enter
Show

Type "# A Mac laptop. Which machines are in my pool?"
Enter
Type "ylcc --json workers | jq -c '.[] | [.id, .capabilities.os, .capabilities.arch]'"
Enter
Sleep 2.5s

Type "# Build and run a Rust program on Windows, bring back its output"
Enter
Type `job=$(ylcc --json run --os windows --tool rustc='*' --input examples/jobs/hello.rs --output result.json -- cmd /c "rustc examples/jobs/hello.rs -o hello.exe && hello.exe" | jq -r .id)`
Enter
Wait /\$$/
Sleep 1s
Type `cat "$(ylcc --json jobs fetch $job | jq -r .results_dir)/result.json"`
Enter
Sleep 4s
