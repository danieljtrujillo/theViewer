' Builds the viewer if the sources changed, then opens it. No console window.
Set shell = CreateObject("WScript.Shell")
Set fso = CreateObject("Scripting.FileSystemObject")

root = fso.GetParentFolderName(WScript.ScriptFullName)
script = root & "\tools\build-and-run.ps1"

target = ""
If WScript.Arguments.Count > 0 Then
  target = " """ & WScript.Arguments(0) & """"
End If

cmd = "powershell -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File """ & script & """" & target
shell.Run cmd, 0, False
