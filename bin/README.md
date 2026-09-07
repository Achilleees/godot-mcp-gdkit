# Server executable

The plugin launches `bin/godot-mcp.exe`. Build and install it from the repository root:

```powershell
./scripts/check.ps1 -GodotBin 'C:/path/to/godot_console.exe' -Install
```

This requires PowerShell 7, Rust, and Godot. The script runs validation before copying
the release executable here. Stop a running server before replacing its executable.

The Windows build statically links the Visual C++ runtime. Generated executables and
debug symbols are excluded from version control.
