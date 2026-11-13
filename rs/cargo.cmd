@echo off
rem Runs cargo with VS 2019 Build Tools: the VS 2022 install on D: has no CRT libs.
call "C:\Program Files (x86)\Microsoft Visual Studio\2019\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul || exit /b 1
"%USERPROFILE%\.cargo\bin\cargo.exe" %*
