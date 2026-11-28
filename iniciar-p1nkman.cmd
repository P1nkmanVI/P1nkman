@echo off
setlocal EnableExtensions DisableDelayedExpansion
if exist "%~dp0iniciar-p1nkman.local.cmd" call "%~dp0iniciar-p1nkman.local.cmd"
if not defined VICTORY_GAME_DIR set "VICTORY_GAME_DIR=C:\Jogos\MeuJogo"
set "VICTORY_DATA_DIR=%LOCALAPPDATA%\P1nkman-Coolfox\data"
set "VICTORY_CACHE_DIR=%LOCALAPPDATA%\P1nkman-Coolfox\cache"
if not "%~1"=="" (echo Este iniciador nao aceita argumentos. & pause & exit /b 2)
if not exist "%VICTORY_GAME_DIR%\game_scarlett_master.exe" (echo Jogo nao encontrado. Edite VICTORY_GAME_DIR no iniciador. & pause & exit /b 1)
if not exist "%~dp0target\release\victory.exe" if not exist "%~dp0target\release\P1nkman.exe" (echo Compile o programa primeiro; veja o README. & pause & exit /b 1)
for /f %%t in ('powershell -NoProfile -Command "Get-Date -Format yyyyMMdd-HHmmss-fff"') do set "STAMP=%%t"
if not defined STAMP (echo Nao foi possivel gerar o nome do log. & pause & exit /b 1)
set "P1_LOG_DIR=%LOCALAPPDATA%\P1nkman-Coolfox\logs"
if not exist "%P1_LOG_DIR%" mkdir "%P1_LOG_DIR%"
if not exist "%P1_LOG_DIR%" (echo Falha ao criar pasta de logs. & pause & exit /b 1)
if exist "%VICTORY_DATA_DIR%\7952987A\saves" (
  robocopy "%VICTORY_DATA_DIR%\7952987A\saves" "%LOCALAPPDATA%\P1nkman-Coolfox\backup-saves\%STAMP%" /E /NFL /NDL /NJH /NJS /NP >nul
  if errorlevel 8 (echo Falha no backup dos saves. & pause & exit /b 1)
)
set "P1_LOG=%P1_LOG_DIR%\p1nkman-%STAMP%.log"
echo P1nkman - perfil Series S. Log: %P1_LOG%
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\capture-session.ps1" -Exe "%VICTORY_GAME_DIR%\game_scarlett_master.exe" -Build release -Console series-s -MaxMinutes 15 -MinFreeGiB 5 > "%P1_LOG%" 2>&1
set "P1_CODE=%ERRORLEVEL%"
echo Encerrado com codigo %P1_CODE%. Log: %P1_LOG%
pause
exit /b %P1_CODE%
