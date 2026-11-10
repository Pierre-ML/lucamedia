; MDL: template NSIS personnalisé (bundle.windows.nsis.template), basé sur le template officiel
; MDL: de tauri-bundler au tag tauri-cli-v2.12.1 (crates/tauri-bundler/src/bundle/windows/nsis/installer.nsi).
; MDL: chaque modification est marquée « ; MDL: ». Ce template produit DEUX exe :
; MDL:  - SETUP (rendu par tauri build, sans define) : première installation UNIQUEMENT. Si Ursa est
; MDL:    déjà installé, message (utiliser l'exe de mise à jour) puis sortie dans .onInit.
; MDL:  - UPDATE (même rendu recompilé par scripts/build-updater.mjs avec /DMDL_UPDATER) : refuse si
; MDL:    Ursa n'est pas installé ou si une version plus récente l'est ; sinon installe par-dessus
; MDL:    l'installation existante (même dossier, pages Installation puis Fin seulement), ce qui répare
; MDL:    aussi une installation abîmée. Rien n'est désinstallé. Lancé par l'app (bouton « mise à
; MDL:    jour », src-tauri/src/updater.rs) avec /MDL_WAITPID=<pid> : .onInit attend d'abord, sans
; MDL:    fenêtre, la fin de ce processus (15 s au plus). En mode graphique, la section Install relance
; MDL:    UNE fois la nouvelle version à la fin de la copie, puis l'update se ferme seul (page de fin
; MDL:    affichée seulement si ce lancement a échoué).
; MDL: Les données utilisateur (%APPDATA% et %LOCALAPPDATA%\${BUNDLEID}) ne sont JAMAIS touchées par
; MDL: l'un ou l'autre ; seule une vraie désinstallation (case « supprimer les données ») les supprime.
; MDL: Supprimés (code mort depuis ce découpage) : la page « déjà installé » (PageReinstall), la « mise à
; MDL: jour propre » (désinstallation de l'ancien programme) et la migration depuis WiX (bundle.targets = nsis).
; MDL: Mode d'installation : currentUser (bundle.windows.nsis.installMode). Installation pour l'utilisateur
; MDL: courant dans %LOCALAPPDATA%\Programs\${PRODUCTNAME}, sans droits administrateur
; MDL: (RequestExecutionLevel user, SetShellVarContext current, SHCTX = HKCU).
; MDL: Sidecars ffmpeg, ffprobe, yt-dlp : les .exe (bundle.externalBin) sont copiés à côté de l'exe principal
; MDL: par la boucle « binaries » de la section Install, et supprimés par celle du désinstallateur.
; MDL: Contrat avec l'app (src-tauri/src/updater.rs) : Ursa-Update.exe /MDL_WAITPID=<pid>.
Unicode true
ManifestDPIAware true
; Add in `dpiAwareness` `PerMonitorV2` to manifest for Windows 10 1607+ (note this should not affect lower versions since they should be able to ignore this and pick up `dpiAware` `true` set by `ManifestDPIAware true`)
; Currently undocumented on NSIS's website but is in the Docs folder of source tree, see
; https://github.com/kichik/nsis/blob/5fc0b87b819a9eec006df4967d08e522ddd651c9/Docs/src/attributes.but#L286-L300
; https://github.com/tauri-apps/tauri/pull/10106
ManifestDPIAwareness PerMonitorV2

!if "{{compression}}" == "none"
  SetCompress off
!else
  ; Set the compression algorithm. We default to LZMA.
  SetCompressor /SOLID "{{compression}}"
!endif

; Keep above !include to stay ahead of any plugin command
; see https://github.com/tauri-apps/tauri/pull/15422#discussion_r3289239624
{{#if signed_plugins_path}}
!addplugindir "{{signed_plugins_path}}"
{{/if}}

!include MUI2.nsh
!include FileFunc.nsh
!include x64.nsh
!include WordFunc.nsh
!include "utils.nsh"
!include "FileAssociation.nsh"
!include "Win\COM.nsh"
!include "Win\Propkey.nsh"
!include "Win\RestartManager.nsh"
!include "StrFunc.nsh"
; MDL: ${StrCase} et ${StrLoc} retirés : ils ne servaient qu'à la détection WiX (supprimée) ;
; MDL: les déclarer sans les utiliser provoque l'avertissement 6010 de makensis.

{{#if installer_hooks}}
!include "{{installer_hooks}}"
{{/if}}

!define WEBVIEW2APPGUID "{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"

!define MANUFACTURER "{{manufacturer}}"
!define PRODUCTNAME "{{product_name}}"
!define VERSION "{{version}}"
!define VERSIONWITHBUILD "{{version_with_build}}"
!define HOMEPAGE "{{homepage}}"
!define INSTALLMODE "{{install_mode}}"
!define LICENSE "{{license}}"
!define INSTALLERICON "{{installer_icon}}"
!define SIDEBARIMAGE "{{sidebar_image}}"
!define HEADERIMAGE "{{header_image}}"
!define UNINSTALLERICON "{{uninstaller_icon}}"
!define UNINSTALLERHEADERIMAGE "{{uninstaller_header_image}}"
!define MAINBINARYNAME "{{main_binary_name}}"
!define MAINBINARYSRCPATH "{{main_binary_path}}"
!define BUNDLEID "{{bundle_id}}"
!define COPYRIGHT "{{copyright}}"
; MDL: l'exe de mise à jour (/DMDL_UPDATER) est écrit dans le fichier passé par
; MDL: /DMDL_UPDATE_OUT=<chemin> (scripts/build-updater.mjs), sinon à côté du setup rendu.
!ifdef MDL_UPDATER
  !ifdef MDL_UPDATE_OUT
    !define OUTFILE "${MDL_UPDATE_OUT}"
  !else
    !define OUTFILE "nsis-output-update.exe"
  !endif
!else
  !define OUTFILE "{{out_file}}"
!endif
!define ARCH "{{arch}}"
!define ADDITIONALPLUGINSPATH "{{additional_plugins_path}}"
!define ALLOWDOWNGRADES "{{allow_downgrades}}"
!define DISPLAYLANGUAGESELECTOR "{{display_language_selector}}"
!define INSTALLWEBVIEW2MODE "{{install_webview2_mode}}"
!define WEBVIEW2INSTALLERARGS "{{webview2_installer_args}}"
!define WEBVIEW2BOOTSTRAPPERPATH "{{webview2_bootstrapper_path}}"
!define WEBVIEW2INSTALLERPATH "{{webview2_installer_path}}"
!define MINIMUMWEBVIEW2VERSION "{{minimum_webview2_version}}"
!define UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCTNAME}"
!define MANUKEY "Software\${MANUFACTURER}"
!define MANUPRODUCTKEY "${MANUKEY}\${PRODUCTNAME}"
!define UNINSTALLERSIGNCOMMAND "{{uninstaller_sign_cmd}}"
!define ESTIMATEDSIZE "{{estimated_size}}"
!define STARTMENUFOLDER "{{start_menu_folder}}"

Var PassiveMode
Var UpdateMode
Var NoShortcutMode
Var WixMode ; MDL: toujours 0 (migration WiX supprimée, voir .onInit)
Var OldMainBinaryName
; MDL: résultat de la détection d'une installation existante (MdlDetectInstall, dans .onInit)
Var MdlInstalled         ; 1 si Ursa est déjà installé (registre ou dossier existant)
Var MdlInstalledVersion  ; version installée (DisplayVersion, vide si inconnue)
Var MdlInstalledDir      ; dossier d'installation existant (sans guillemets ni « \ » final)
Var MdlTmp               ; brouillon
!ifdef MDL_UPDATER
; MDL: update : 1 si la section Install a déjà lancé la nouvelle version (page de fin sautée)
Var MdlLaunched
!endif

; MDL: lance l'app installée. installMode currentUser : l'installateur n'est pas élevé, Exec
; MDL: (CreateProcess direct) suffit. nsis_tauri_utils::RunAsUser (CreateProcessWithTokenW avec le
; MDL: jeton d'explorer, appel au service seclogon) ne sert qu'à quitter l'élévation : sur la page de
; MDL: fin, cet appel bloquait la fenêtre (« Ursa-Update.exe a cessé d'interagir », AppHangXProcB1)
; MDL: et laissait un ursa.exe à moitié créé. Conservé pour les autres modes d'installation.
!macro MDL_RUN_APP
  !if "${INSTALLMODE}" == "currentUser"
    Exec '"$INSTDIR\${MAINBINARYNAME}.exe"'
  !else
    nsis_tauri_utils::RunAsUser "$INSTDIR\${MAINBINARYNAME}.exe" ""
  !endif
!macroend

; MDL: retire les guillemets de début et de fin d'un chemin lu dans le registre (ex. InstallLocation
; MDL: est écrit "%LOCALAPPDATA%\Programs\Ursa" développé, AVEC guillemets). VAR ne doit pas être $MdlTmp.
; MDL: Défini ici (et non avec les autres fonctions Mdl) car utilisé dès RestorePreviousInstallLocation.
!macro MDL_UNQUOTE VAR
  StrCpy $MdlTmp ${VAR} 1
  ${If} $MdlTmp == "$\""
    StrCpy ${VAR} ${VAR} "" 1
  ${EndIf}
  StrCpy $MdlTmp ${VAR} "" -1
  ${If} $MdlTmp == "$\""
    StrCpy ${VAR} ${VAR} -1
  ${EndIf}
!macroend

; MDL: double lancement (.onInit) : cherche une fenêtre de premier niveau de classe « #32770 » dont
; MDL: le titre est exactement TITLE, ou TITLE suivi d'une espace (pages MUI2). $MdlTmp reçoit
; MDL: le handle trouvé (0 sinon) ; ne cherche plus dès qu'une fenêtre a été trouvée.
!macro MDL_FIND_INSTALLER_WINDOW TITLE
  ${If} $MdlTmp = 0
    FindWindow $MdlTmp "#32770" "${TITLE}"
  ${EndIf}
  ${If} $MdlTmp = 0
    FindWindow $MdlTmp "#32770" "${TITLE} "
  ${EndIf}
!macroend

; MDL: retire le « \ » final éventuel d'un chemin (VAR ne doit pas être $MdlTmp)
!macro MDL_STRIP_TRAILING_SLASH VAR
  StrCpy $MdlTmp ${VAR} "" -1
  ${If} $MdlTmp == "\"
    StrCpy ${VAR} ${VAR} -1
  ${EndIf}
!macroend

Name "${PRODUCTNAME}"
BrandingText "${COPYRIGHT}"
OutFile "${OUTFILE}"
; MDL: titre de la fenêtre de l'exe de mise à jour (« Mise à jour de Ursa »)
!ifdef MDL_UPDATER
  Caption "$(mdlUpdateTitle)"
!endif

; We don't actually use this value as default install path,
; it's just for nsis to append the product name folder in the directory selector
; https://nsis.sourceforge.io/Reference/InstallDir
!define PLACEHOLDER_INSTALL_DIR "placeholder\${PRODUCTNAME}"
InstallDir "${PLACEHOLDER_INSTALL_DIR}"

VIProductVersion "${VERSIONWITHBUILD}"
VIAddVersionKey "ProductName" "${PRODUCTNAME}"
VIAddVersionKey "FileDescription" "${PRODUCTNAME}"
VIAddVersionKey "LegalCopyright" "${COPYRIGHT}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"

# additional plugins
!addplugindir "${ADDITIONALPLUGINSPATH}"

; Uninstaller signing command
!if "${UNINSTALLERSIGNCOMMAND}" != ""
  !uninstfinalize '${UNINSTALLERSIGNCOMMAND}'
!endif

; Handle install mode, `perUser`, `perMachine` or `both`
!if "${INSTALLMODE}" == "perMachine"
  RequestExecutionLevel admin
!endif

!if "${INSTALLMODE}" == "currentUser"
  RequestExecutionLevel user
!endif

!if "${INSTALLMODE}" == "both"
  !define MULTIUSER_MUI
  !define MULTIUSER_INSTALLMODE_INSTDIR "${PRODUCTNAME}"
  !define MULTIUSER_INSTALLMODE_COMMANDLINE
  !if "${ARCH}" == "x64"
    !define MULTIUSER_USE_PROGRAMFILES64
  !else if "${ARCH}" == "arm64"
    !define MULTIUSER_USE_PROGRAMFILES64
  !endif
  !define MULTIUSER_INSTALLMODE_DEFAULT_REGISTRY_KEY "${UNINSTKEY}"
  !define MULTIUSER_INSTALLMODE_DEFAULT_REGISTRY_VALUENAME "CurrentUser"
  !define MULTIUSER_INSTALLMODEPAGE_SHOWUSERNAME
  !define MULTIUSER_INSTALLMODE_FUNCTION RestorePreviousInstallLocation
  !define MULTIUSER_EXECUTIONLEVEL Highest
  !include MultiUser.nsh
!endif

; Installer icon
!if "${INSTALLERICON}" != ""
  !define MUI_ICON "${INSTALLERICON}"
!endif

; Installer sidebar image
!if "${SIDEBARIMAGE}" != ""
  !define MUI_WELCOMEFINISHPAGE_BITMAP "${SIDEBARIMAGE}"
!endif

; Enable header images for installer and uninstaller pages when either image is configured.
!if "${HEADERIMAGE}" != ""
  !define MUI_HEADERIMAGE
!else if "${UNINSTALLERHEADERIMAGE}" != ""
  !define MUI_HEADERIMAGE
!endif

; Installer header image
!if "${HEADERIMAGE}" != ""
  !define MUI_HEADERIMAGE_BITMAP "${HEADERIMAGE}"
!endif

; Uninstaller header image
!if "${UNINSTALLERHEADERIMAGE}" != ""
  !define MUI_HEADERIMAGE_UNBITMAP "${UNINSTALLERHEADERIMAGE}"
!endif

; Uninstaller icon
!if "${UNINSTALLERICON}" != ""
  !define MUI_UNICON "${UNINSTALLERICON}"
!endif

; Define registry key to store installer language
!define MUI_LANGDLL_REGISTRY_ROOT "HKCU"
!define MUI_LANGDLL_REGISTRY_KEY "${MANUPRODUCTKEY}"
!define MUI_LANGDLL_REGISTRY_VALUENAME "Installer Language"

; Installer pages, must be ordered as they appear
; MDL: SETUP : pages d'origine (Bienvenue, Licence, Dossier, Menu démarrer, Installation, Fin), toutes
; MDL: sur SkipIfPassive comme dans le template officiel : le setup ne tourne plus jamais sur une
; MDL: installation existante (refus dans .onInit).
; MDL: UPDATE (/DMDL_UPDATER) : seulement Installation puis Fin. Le dossier est imposé (celui de
; MDL: l'installation existante, fixé dans .onInit) et ne peut pas être changé.
!ifndef MDL_UPDATER
; 1. Welcome Page
!define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive
!insertmacro MUI_PAGE_WELCOME

; 2. License Page (if defined)
!if "${LICENSE}" != ""
  !define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive
  !insertmacro MUI_PAGE_LICENSE "${LICENSE}"
!endif

; 3. Install mode (if it is set to `both`)
!if "${INSTALLMODE}" == "both"
  !define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive
  !insertmacro MULTIUSER_PAGE_INSTALLMODE
!endif

; 4. Custom page to ask user if he wants to reinstall/uninstall
;    only if a previous installation was detected
; MDL: page supprimée : le setup refuse une installation existante dans .onInit,
; MDL: et l'update n'a aucune page de choix.

; 5. Choose install directory page
!define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive
!insertmacro MUI_PAGE_DIRECTORY
!endif ; MDL: fin des pages réservées au setup

; 6. Start menu shortcut page
; MDL: dans l'update, page déclarée mais toujours sautée (Skip) : les macros MUI_STARTMENU_*
; MDL: de la section Install et du désinstallateur en ont besoin (dossier par défaut).
Var AppStartMenuFolder
!if "${STARTMENUFOLDER}" != ""
  !ifdef MDL_UPDATER
    !define MUI_PAGE_CUSTOMFUNCTION_PRE Skip
  !else
    !define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive
  !endif
  !define MUI_STARTMENUPAGE_DEFAULTFOLDER "${STARTMENUFOLDER}"
!else
  !define MUI_PAGE_CUSTOMFUNCTION_PRE Skip
!endif
!insertmacro MUI_PAGE_STARTMENU Application $AppStartMenuFolder

; 7. Installation page
; MDL: en-têtes adaptés dans l'update (pendant puis après la copie des fichiers)
!ifdef MDL_UPDATER
  !define MUI_PAGE_HEADER_TEXT "$(mdlUpdateTitle)"
  !define MUI_PAGE_HEADER_SUBTEXT "$(mdlUpdateSubtitle)"
  !define MUI_INSTFILESPAGE_FINISHHEADER_TEXT "$(mdlUpdateDoneTitle)"
  !define MUI_INSTFILESPAGE_FINISHHEADER_SUBTEXT "$(mdlUpdateDoneSubtitle)"
!endif
!insertmacro MUI_PAGE_INSTFILES

; 8. Finish page
;
; Don't auto jump to finish page after installation page,
; because the installation page has useful info that can be used debug any issues with the installer.
!define MUI_FINISHPAGE_NOAUTOCLOSE
; MDL: case « raccourci Bureau » seulement dans le setup. L'update n'en a pas (elle n'est pas créée
; MDL: du tout, plutôt que masquée) : un raccourci Bureau existant est mis à jour par la section
; MDL: Install (MdlUpdateDesktopShortcut), et aucun n'est créé s'il n'existait pas.
!ifndef MDL_UPDATER
; Use show readme button in the finish page as a button create a desktop shortcut
!define MUI_FINISHPAGE_SHOWREADME
!define MUI_FINISHPAGE_SHOWREADME_TEXT "$(createDesktop)"
; MDL: fonction intermédiaire (au lieu de CreateOrUpdateDesktopShortcut) : ne fait rien si
; MDL: RunMainBinary a déjà créé le raccourci (voir $MdlDesktopDone ci-dessous)
!define MUI_FINISHPAGE_SHOWREADME_FUNCTION MdlFinishDesktopShortcut
; MDL: 1 si le raccourci Bureau a déjà été créé par RunMainBinary sur la page de fin
Var MdlDesktopDone
!else
  !define MUI_FINISHPAGE_TITLE "$(mdlUpdateDoneTitle)"
  !define MUI_FINISHPAGE_TEXT "$(mdlUpdateDoneText)"
!endif
; Show run app after installation.
; MDL: case « Lancer » cochée par défaut dans les deux exe (pas de MUI_FINISHPAGE_RUN_NOTCHECKED).
; MDL: Setup : seul chemin de lancement en mode graphique. Update : la section Install lance déjà
; MDL: la nouvelle version et la page de fin est alors sautée (MdlUpdateFinishPre) ; elle ne
; MDL: s'affiche, avec cette case, que si ce lancement a échoué. .onInstSuccess ne lance qu'en
; MDL: passif/silencieux avec /R.
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_FUNCTION RunMainBinary
!ifdef MDL_UPDATER
  !define MUI_PAGE_CUSTOMFUNCTION_PRE MdlUpdateFinishPre
!else
  !define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive
!endif
!insertmacro MUI_PAGE_FINISH

!ifdef MDL_UPDATER
; MDL: page de fin de l'update sautée en passif (comme SkipIfPassive) ou si la nouvelle version a
; MDL: déjà été lancée par la section Install : l'update se termine alors sans clic.
Function MdlUpdateFinishPre
  Call SkipIfPassive ; Abort dans une fonction appelée : la page est quand même sautée
  ${If} $MdlLaunched = 1
    Abort
  ${EndIf}
FunctionEnd
!endif

; MDL: ordre garanti « raccourci PUIS lancement ». Dans la fonction de sortie de la page de fin
; MDL: (Modern UI 2\Pages\Finish.nsh), MUI appelle MUI_FINISHPAGE_RUN_FUNCTION AVANT
; MDL: MUI_FINISHPAGE_SHOWREADME_FUNCTION : l'app était lancée avant la création du raccourci et
; MDL: plantait. Setup : si la case « raccourci Bureau » est cochée, RunMainBinary crée d'abord le
; MDL: raccourci, met $MdlDesktopDone à 1 (MdlFinishDesktopShortcut, appelée ensuite par MUI,
; MDL: ne refait rien), attend 300 ms, puis lance l'app. La fenêtre de la page existe encore ici
; MDL: (MUI lit lui-même l'état des cases à ce moment). Update : pas de case raccourci, lancement direct.
Function RunMainBinary
  !ifndef MDL_UPDATER
    ${If} $mui.FinishPage.ShowReadme <> 0
      SendMessage $mui.FinishPage.ShowReadme ${BM_GETCHECK} 0 0 $MdlTmp
      ${If} $MdlTmp = ${BST_CHECKED}
        Call CreateOrUpdateDesktopShortcut
        StrCpy $MdlDesktopDone 1
        Sleep 300
      ${EndIf}
    ${EndIf}
  !endif
  !insertmacro MDL_RUN_APP ; MDL: au lieu de nsis_tauri_utils::RunAsUser (voir la macro)
FunctionEnd

!ifndef MDL_UPDATER
; MDL: action de la case « raccourci Bureau » (appelée par MUI après RunMainBinary) : crée le
; MDL: raccourci, sauf s'il vient déjà d'être créé par RunMainBinary
Function MdlFinishDesktopShortcut
  ${If} $MdlDesktopDone = 1
    Return
  ${EndIf}
  Call CreateOrUpdateDesktopShortcut
  StrCpy $MdlDesktopDone 1
FunctionEnd
!endif

; Uninstaller Pages
; 1. Confirm uninstall page
Var DeleteAppDataCheckbox
Var DeleteAppDataCheckboxState
!define /ifndef WS_EX_LAYOUTRTL         0x00400000
!define MUI_PAGE_CUSTOMFUNCTION_SHOW un.ConfirmShow
Function un.ConfirmShow ; Add add a `Delete app data` check box
  ; $1 inner dialog HWND
  ; $2 window DPI
  ; $3 style
  ; $4 x
  ; $5 y
  ; $6 width
  ; $7 height
  FindWindow $1 "#32770" "" $HWNDPARENT ; Find inner dialog
  System::Call "user32::GetDpiForWindow(p r1) i .r2"
  ${If} $(^RTL) = 1
    StrCpy $3 "${__NSD_CheckBox_EXSTYLE} | ${WS_EX_LAYOUTRTL}"
    IntOp $4 50 * $2
  ${Else}
    StrCpy $3 "${__NSD_CheckBox_EXSTYLE}"
    IntOp $4 0 * $2
  ${EndIf}
  IntOp $5 100 * $2
  IntOp $6 400 * $2
  IntOp $7 25 * $2
  IntOp $4 $4 / 96
  IntOp $5 $5 / 96
  IntOp $6 $6 / 96
  IntOp $7 $7 / 96
  System::Call 'user32::CreateWindowEx(i r3, w "${__NSD_CheckBox_CLASS}", w "$(deleteAppData)", i ${__NSD_CheckBox_STYLE}, i r4, i r5, i r6, i r7, p r1, i0, i0, i0) i .s'
  Pop $DeleteAppDataCheckbox
  SendMessage $HWNDPARENT ${WM_GETFONT} 0 0 $1
  SendMessage $DeleteAppDataCheckbox ${WM_SETFONT} $1 1
FunctionEnd
!define MUI_PAGE_CUSTOMFUNCTION_LEAVE un.ConfirmLeave
Function un.ConfirmLeave
  SendMessage $DeleteAppDataCheckbox ${BM_GETCHECK} 0 0 $DeleteAppDataCheckboxState
FunctionEnd
!define MUI_PAGE_CUSTOMFUNCTION_PRE un.SkipIfPassive
!insertmacro MUI_UNPAGE_CONFIRM

; 2. Uninstalling Page
!insertmacro MUI_UNPAGE_INSTFILES

;Languages
{{#each languages}}
!insertmacro MUI_LANGUAGE "{{this}}"
{{/each}}
!insertmacro MUI_RESERVEFILE_LANGDLL
{{#each language_files}}
  !include "{{this}}"
{{/each}}

; MDL: textes Ursa (bundle.windows.nsis.languages = French, English ; langue choisie
; MDL: automatiquement selon celle de Windows). $MdlInstalledVersion est remplacé à l'exécution
; MDL: par la version installée. Chaque texte n'est compilé que dans l'exe qui l'utilise.
!ifdef LANG_FRENCH
; MDL: double lancement (setup et update se détectent mutuellement, voir .onInit)
LangString mdlSetupAlreadyRunning ${LANG_FRENCH} "Une installation ou une mise à jour de ${PRODUCTNAME} est déjà en cours."
; MDL: remplace les textes NSIS intégrés ^FileError (Abandonner/Recommencer/Ignorer) et
; MDL: ^FileError_NoIgnore (Recommencer/Annuler) affichés quand un fichier ne peut pas être écrit
; MDL: (antivirus qui bloque l'exe non signé). Un LangString défini dans le script a priorité sur le
; MDL: texte du fichier .nlf (vérifié avec makensis 3.11 : le texte d'origine disparaît de l'exe).
; MDL: Au moment du message, NSIS place le chemin complet du fichier dans $0 (même jeton que
; MDL: French.nlf / English.nlf). Communs au setup et à l'update.
LangString ^FileError ${LANG_FRENCH} "Impossible d'écrire le fichier :$\r$\n$\r$\n$0$\r$\n$\r$\nVotre antivirus bloque probablement l'installation de ${PRODUCTNAME} (le programme n'est pas encore signé). Ajoutez une exception pour ce programme d'installation et pour le dossier $INSTDIR, puis cliquez sur Recommencer.$\r$\n$\r$\nAbandonner annule l'installation. Ignorer passe ce fichier : ${PRODUCTNAME} serait alors incomplet."
LangString ^FileError_NoIgnore ${LANG_FRENCH} "Impossible d'écrire le fichier :$\r$\n$\r$\n$0$\r$\n$\r$\nVotre antivirus bloque probablement l'installation de ${PRODUCTNAME} (le programme n'est pas encore signé). Ajoutez une exception pour ce programme d'installation et pour le dossier $INSTDIR, puis cliquez sur Recommencer.$\r$\n$\r$\nAnnuler arrête l'installation."
!ifndef MDL_UPDATER
; MDL: setup lancé alors que Ursa est déjà installé
LangString mdlAlreadyInstalledUseUpdate ${LANG_FRENCH} "${PRODUCTNAME} est déjà installé sur cet ordinateur.$\r$\n$\r$\nPour le mettre à jour, utilisez le fichier Ursa-Update.exe (disponible sur la page des versions, à côté de ce programme d'installation)."
!else
; MDL: update : refus (pas installé, version plus récente), titres des pages
LangString mdlNotInstalledUseSetup ${LANG_FRENCH} "${PRODUCTNAME} n'est pas installé sur cet ordinateur.$\r$\n$\r$\nUtilisez d'abord le programme d'installation Ursa-Setup.exe (disponible sur la page des versions)."
LangString mdlNewerInstalled ${LANG_FRENCH} "Une version plus récente de ${PRODUCTNAME} ($MdlInstalledVersion) est déjà installée.$\r$\n$\r$\nCette mise à jour (version ${VERSION}) n'est pas nécessaire."
LangString mdlUpdateTitle ${LANG_FRENCH} "Mise à jour de ${PRODUCTNAME}"
LangString mdlUpdateSubtitle ${LANG_FRENCH} "Installation de la version ${VERSION}. Vos réglages et vos sessions sont conservés."
LangString mdlUpdateDoneTitle ${LANG_FRENCH} "Mise à jour terminée"
LangString mdlUpdateDoneSubtitle ${LANG_FRENCH} "${PRODUCTNAME} est à jour (version ${VERSION})."
LangString mdlUpdateDoneText ${LANG_FRENCH} "${PRODUCTNAME} a été mis à jour vers la version ${VERSION}. Vos réglages et vos sessions ont été conservés.$\r$\n$\r$\nCliquez sur Fermer pour quitter."
; MDL: ligne du journal pendant le lancement de la nouvelle version (section Install)
LangString mdlUpdateLaunching ${LANG_FRENCH} "Démarrage de ${PRODUCTNAME} ${VERSION}..."
!endif
!endif

!ifdef LANG_ENGLISH
; MDL: double lancement (setup et update se détectent mutuellement, voir .onInit)
LangString mdlSetupAlreadyRunning ${LANG_ENGLISH} "An installation or update of ${PRODUCTNAME} is already running."
; MDL: message d'écriture impossible (voir le bloc français ci-dessus)
LangString ^FileError ${LANG_ENGLISH} "Unable to write the file:$\r$\n$\r$\n$0$\r$\n$\r$\nYour antivirus is probably blocking the installation of ${PRODUCTNAME} (the program is not signed yet). Add an exception for this installer and for the folder $INSTDIR, then click Retry.$\r$\n$\r$\nAbort cancels the installation. Ignore skips this file: ${PRODUCTNAME} would then be incomplete."
LangString ^FileError_NoIgnore ${LANG_ENGLISH} "Unable to write the file:$\r$\n$\r$\n$0$\r$\n$\r$\nYour antivirus is probably blocking the installation of ${PRODUCTNAME} (the program is not signed yet). Add an exception for this installer and for the folder $INSTDIR, then click Retry.$\r$\n$\r$\nCancel stops the installation."
!ifndef MDL_UPDATER
; MDL: setup lancé alors que Ursa est déjà installé
LangString mdlAlreadyInstalledUseUpdate ${LANG_ENGLISH} "${PRODUCTNAME} is already installed on this computer.$\r$\n$\r$\nTo update it, use the file Ursa-Update.exe (available on the releases page, next to this installer)."
!else
; MDL: update : refus (pas installé, version plus récente), titres des pages
LangString mdlNotInstalledUseSetup ${LANG_ENGLISH} "${PRODUCTNAME} is not installed on this computer.$\r$\n$\r$\nPlease use the installer Ursa-Setup.exe first (available on the releases page)."
LangString mdlNewerInstalled ${LANG_ENGLISH} "A newer version of ${PRODUCTNAME} ($MdlInstalledVersion) is already installed.$\r$\n$\r$\nThis update (version ${VERSION}) is not needed."
LangString mdlUpdateTitle ${LANG_ENGLISH} "${PRODUCTNAME} Update"
LangString mdlUpdateSubtitle ${LANG_ENGLISH} "Installing version ${VERSION}. Your settings and sessions are kept."
LangString mdlUpdateDoneTitle ${LANG_ENGLISH} "Update complete"
LangString mdlUpdateDoneSubtitle ${LANG_ENGLISH} "${PRODUCTNAME} is up to date (version ${VERSION})."
LangString mdlUpdateDoneText ${LANG_ENGLISH} "${PRODUCTNAME} has been updated to version ${VERSION}. Your settings and sessions have been kept.$\r$\n$\r$\nClick Close to exit."
LangString mdlUpdateLaunching ${LANG_ENGLISH} "Starting ${PRODUCTNAME} ${VERSION}..."
!endif
!endif

Function .onInit
  ; MDL: un seul setup OU update à la fois, détection « au mieux » SANS appel système : l'ancien
  ; MDL: mutex nommé « Global\... » (créé par un appel système via le plugin System) a été retiré, car Norton le
  ; MDL: classait comme comportement suspect (IDP.Generic) dans un exe élevé non signé.
  ; MDL: Instruction NSIS native FindWindow (pas de plugin) : la fenêtre principale d'un installateur
  ; MDL: NSIS est une boîte de dialogue de premier niveau de classe « #32770 », dont le titre est la
  ; MDL: Caption de l'exe, suivie d'une espace sur les pages MUI2 (Caption " " de page) et sans
  ; MDL: espace sur la page Bienvenue. Titres testés (titre exact, avec et sans espace finale) :
  ; MDL:   setup  : « Installation de Ursa » (French.nlf ^SetupCaption), « Ursa Setup » (English.nlf)
  ; MDL:   update : « Mise à jour de Ursa », « Ursa Update » (LangString mdlUpdateTitle)
  ; MDL: Le setup et l'update se détectent donc mutuellement, quelle que soit la langue. Le
  ; MDL: désinstallateur (« Désinstallation de Ursa » / « Ursa Uninstall ») n'est PAS bloquant.
  ; MDL: Pendant .onInit, la fenêtre de l'instance courante n'est pas encore créée : FindWindow ne peut
  ; MDL: pas la trouver elle-même. Limites acceptées : une instance silencieuse (/S, sans fenêtre)
  ; MDL: n'est pas détectée, et une MessageBox d'une autre instance (même titre) l'est.
  ; MDL: Si les titres changent (Caption, mdlUpdateTitle), mettre à jour la liste ci-dessous.
  StrCpy $MdlTmp 0
  !insertmacro MDL_FIND_INSTALLER_WINDOW "Installation de ${PRODUCTNAME}"
  !insertmacro MDL_FIND_INSTALLER_WINDOW "${PRODUCTNAME} Setup"
  !insertmacro MDL_FIND_INSTALLER_WINDOW "Mise à jour de ${PRODUCTNAME}"
  !insertmacro MDL_FIND_INSTALLER_WINDOW "${PRODUCTNAME} Update"
  ${If} $MdlTmp <> 0
    MessageBox MB_ICONINFORMATION|MB_OK "$(mdlSetupAlreadyRunning)" /SD IDOK
    Abort
  ${EndIf}

  StrCpy $WixMode 0 ; MDL: migration WiX supprimée (évite aussi l'avertissement 6001 de makensis)

  ${GetOptions} $CMDLINE "/P" $PassiveMode
  ${IfNot} ${Errors}
    StrCpy $PassiveMode 1
  ${EndIf}

  ${GetOptions} $CMDLINE "/NS" $NoShortcutMode
  ${IfNot} ${Errors}
    StrCpy $NoShortcutMode 1
  ${EndIf}

  ${GetOptions} $CMDLINE "/UPDATE" $UpdateMode
  ${IfNot} ${Errors}
    StrCpy $UpdateMode 1
  ${EndIf}

  !if "${DISPLAYLANGUAGESELECTOR}" == "true"
    !insertmacro MUI_LANGDLL_DISPLAY
  !endif

  !insertmacro SetContext

  ${If} $INSTDIR == "${PLACEHOLDER_INSTALL_DIR}"
    ; Set default install location
    !if "${INSTALLMODE}" == "perMachine"
      ${If} ${RunningX64}
        !if "${ARCH}" == "x64"
          StrCpy $INSTDIR "$PROGRAMFILES64\${PRODUCTNAME}"
        !else if "${ARCH}" == "arm64"
          StrCpy $INSTDIR "$PROGRAMFILES64\${PRODUCTNAME}"
        !else
          StrCpy $INSTDIR "$PROGRAMFILES\${PRODUCTNAME}"
        !endif
      ${Else}
        StrCpy $INSTDIR "$PROGRAMFILES\${PRODUCTNAME}"
      ${EndIf}
    !else if "${INSTALLMODE}" == "currentUser"
      ; MDL: %LOCALAPPDATA%\Programs\${PRODUCTNAME} (emplacement standard des applications installées
      ; MDL: pour l'utilisateur, comme VS Code ou Discord) au lieu de $LOCALAPPDATA\${PRODUCTNAME}.
      ; MDL: Distinct du dossier de données $LOCALAPPDATA\${BUNDLEID} (cache WebView2).
      StrCpy $INSTDIR "$LOCALAPPDATA\Programs\${PRODUCTNAME}"
    !endif

    Call RestorePreviousInstallLocation
  ${EndIf}


  !if "${INSTALLMODE}" == "both"
    !insertmacro MULTIUSER_INIT
  !endif

  ; MDL: détection d'une installation existante, puis aiguillage setup / update.
  ; MDL: Les Abort sont faits ici, directement dans .onInit (l'installateur quitte sans page).
  ; MDL: installMode currentUser : SetContext ci-dessus a fait SetShellVarContext current, donc
  ; MDL: MdlDetectInstall (SHCTX) ne voit QUE l'installation de l'utilisateur courant (HKCU).
  ; MDL: Une éventuelle installation perMachine (HKLM) n'est donc pas considérée comme « déjà installée ».
  Call MdlDetectInstall
  !ifdef MDL_UPDATER
    ; MDL: UPDATE lancé par l'app (src-tauri/src/updater.rs) avec /MDL_WAITPID=<pid> : l'app se
    ; MDL: ferme. On attend la fin de CE processus (par son PID, pas par son nom) pendant 15 s au plus, AVANT d'afficher quoi que ce soit : aucune
    ; MDL: fenêtre ni console. Processus déjà terminé : OpenProcess échoue, pas d'attente. Au-delà de
    ; MDL: 15 s, CheckIfAppIsRunning (section Install) prend le relais comme avant.
    ${GetOptions} $CMDLINE "/MDL_WAITPID=" $MdlTmp
    ${IfNot} ${Errors}
      IntOp $MdlTmp $MdlTmp + 0
      ${If} $MdlTmp > 0
        Push $0
        Push $1
        System::Call 'kernel32::OpenProcess(i 0x00100000, i 0, i $MdlTmp) p .r0' ; SYNCHRONIZE
        ${If} $0 P<> 0
          System::Call 'kernel32::WaitForSingleObject(p r0, i 15000) i .r1'
          System::Call 'kernel32::CloseHandle(p r0)'
        ${EndIf}
        Pop $1
        Pop $0
      ${EndIf}
    ${EndIf}
    ; MDL: UPDATE : Ursa doit être installé
    ${If} $MdlInstalled <> 1
      MessageBox MB_ICONINFORMATION|MB_OK "$(mdlNotInstalledUseSetup)" /SD IDOK
      Abort
    ${EndIf}
    ; MDL: refus si la version installée est plus récente (SemverCompare : -1 = ${VERSION} plus
    ; MDL: ancienne). Même version : autorisé (répare). Version inconnue (DisplayVersion absent,
    ; MDL: installation abîmée) : autorisé (répare).
    ${If} $MdlInstalledVersion != ""
      nsis_tauri_utils::SemverCompare "${VERSION}" $MdlInstalledVersion
      Pop $MdlTmp
      ${If} $MdlTmp = -1
        MessageBox MB_ICONINFORMATION|MB_OK "$(mdlNewerInstalled)" /SD IDOK
        Abort
      ${EndIf}
    ${EndIf}
    ; MDL: dossier imposé = dossier existant (ignore /D= ; pas de page Dossier). S'il est inconnu
    ; MDL: (registre très abîmé), on garde le dossier par défaut calculé ci-dessus.
    ${If} $MdlInstalledDir != ""
      StrCpy $INSTDIR $MdlInstalledDir
    ${EndIf}
  !else
    ; MDL: SETUP : première installation uniquement
    ${If} $MdlInstalled = 1
      MessageBox MB_ICONINFORMATION|MB_OK "$(mdlAlreadyInstalledUseUpdate)" /SD IDOK
      Abort
    ${EndIf}
  !endif
FunctionEnd


Section EarlyChecks
  ; Abort silent installer if downgrades is disabled
  !if "${ALLOWDOWNGRADES}" == "false"
  ${If} ${Silent}
    ; If downgrading
    ${If} $R0 = -1
      System::Call 'kernel32::AttachConsole(i -1)i.r0'
      ${If} $0 <> 0
        System::Call 'kernel32::GetStdHandle(i -11)i.r0'
        System::call 'kernel32::SetConsoleTextAttribute(i r0, i 0x0004)' ; set red color
        FileWrite $0 "$(silentDowngrades)"
      ${EndIf}
      Abort
    ${EndIf}
  ${EndIf}
  !endif

SectionEnd

Section WebView2
  ; Check if Webview2 is already installed and skip this section
  ${If} ${RunningX64}
    ReadRegStr $4 HKLM "SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\${WEBVIEW2APPGUID}" "pv"
  ${Else}
    ReadRegStr $4 HKLM "SOFTWARE\Microsoft\EdgeUpdate\Clients\${WEBVIEW2APPGUID}" "pv"
  ${EndIf}
  ${If} $4 == ""
    ReadRegStr $4 HKCU "SOFTWARE\Microsoft\EdgeUpdate\Clients\${WEBVIEW2APPGUID}" "pv"
  ${EndIf}

  ${If} $4 == ""
    ; Webview2 installation
    ;
    ; Skip if updating
    ${If} $UpdateMode <> 1
      !if "${INSTALLWEBVIEW2MODE}" == "downloadBootstrapper"
        Delete "$TEMP\MicrosoftEdgeWebview2Setup.exe"
        DetailPrint "$(webview2Downloading)"
        NSISdl::download "https://go.microsoft.com/fwlink/p/?LinkId=2124703" "$TEMP\MicrosoftEdgeWebview2Setup.exe"
        Pop $0
        ${If} $0 == "success"
          DetailPrint "$(webview2DownloadSuccess)"
        ${Else}
          DetailPrint "$(webview2DownloadError)"
          Abort "$(webview2AbortError)"
        ${EndIf}
        StrCpy $6 "$TEMP\MicrosoftEdgeWebview2Setup.exe"
        Goto install_webview2
      !endif

      !if "${INSTALLWEBVIEW2MODE}" == "embedBootstrapper"
        Delete "$TEMP\MicrosoftEdgeWebview2Setup.exe"
        File "/oname=$TEMP\MicrosoftEdgeWebview2Setup.exe" "${WEBVIEW2BOOTSTRAPPERPATH}"
        DetailPrint "$(installingWebview2)"
        StrCpy $6 "$TEMP\MicrosoftEdgeWebview2Setup.exe"
        Goto install_webview2
      !endif

      !if "${INSTALLWEBVIEW2MODE}" == "offlineInstaller"
        Delete "$TEMP\MicrosoftEdgeWebView2RuntimeInstaller.exe"
        File "/oname=$TEMP\MicrosoftEdgeWebView2RuntimeInstaller.exe" "${WEBVIEW2INSTALLERPATH}"
        DetailPrint "$(installingWebview2)"
        StrCpy $6 "$TEMP\MicrosoftEdgeWebView2RuntimeInstaller.exe"
        Goto install_webview2
      !endif

      Goto webview2_done

      install_webview2:
        DetailPrint "$(installingWebview2)"
        ; $6 holds the path to the webview2 installer
        ExecWait "$6 ${WEBVIEW2INSTALLERARGS} /install" $1
        ${If} $1 = 0
          DetailPrint "$(webview2InstallSuccess)"
        ${Else}
          DetailPrint "$(webview2InstallError)"
          Abort "$(webview2AbortError)"
        ${EndIf}
      webview2_done:
    ${EndIf}
  ${Else}
    !if "${MINIMUMWEBVIEW2VERSION}" != ""
      ${VersionCompare} "${MINIMUMWEBVIEW2VERSION}" "$4" $R0
      ${If} $R0 = 1
        update_webview:
          DetailPrint "$(installingWebview2)"
          ${If} ${RunningX64}
            ReadRegStr $R1 HKLM "SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate" "path"
          ${Else}
            ReadRegStr $R1 HKLM "SOFTWARE\Microsoft\EdgeUpdate" "path"
          ${EndIf}
          ${If} $R1 == ""
            ReadRegStr $R1 HKCU "SOFTWARE\Microsoft\EdgeUpdate" "path"
          ${EndIf}
          ${If} $R1 != ""
            ; Chromium updater docs: https://source.chromium.org/chromium/chromium/src/+/main:docs/updater/user_manual.md
            ; Modified from "HKEY_LOCAL_MACHINE\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\Microsoft EdgeWebView\ModifyPath"
            ExecWait `"$R1" /install appguid=${WEBVIEW2APPGUID}&needsadmin=true` $1
            ${If} $1 = 0
              DetailPrint "$(webview2InstallSuccess)"
            ${Else}
              MessageBox MB_ICONEXCLAMATION|MB_ABORTRETRYIGNORE "$(webview2InstallError)" IDIGNORE ignore IDRETRY update_webview
              Quit
              ignore:
            ${EndIf}
          ${EndIf}
      ${EndIf}
    !endif
  ${EndIf}
SectionEnd

Section Install
  ; MDL: section commune au setup et à l'update : copie par-dessus, réécrit uninstall.exe et les
  ; MDL: clés du registre (répare une installation abîmée), ne désinstalle rien.
  SetOutPath $INSTDIR

  !ifmacrodef NSIS_HOOK_PREINSTALL
    !insertmacro NSIS_HOOK_PREINSTALL
  !endif

  !insertmacro CheckIfAppIsRunning "$INSTDIR\${MAINBINARYNAME}.exe" "${PRODUCTNAME}"
  ; MDL: sidecars ffmpeg.exe, ffprobe.exe, yt-dlp.exe : un process encore en cours (téléchargement lancé
  ; MDL: par l'app tuée de force) verrouillerait le fichier. Vérifiés par leur CHEMIN COMPLET (Restart Manager).
  ${If} ${FileExists} "$INSTDIR\ffmpeg.exe"
    !insertmacro CheckIfAppIsRunning "$INSTDIR\ffmpeg.exe" "${PRODUCTNAME}"
  ${EndIf}
  ${If} ${FileExists} "$INSTDIR\ffprobe.exe"
    !insertmacro CheckIfAppIsRunning "$INSTDIR\ffprobe.exe" "${PRODUCTNAME}"
  ${EndIf}
  ${If} ${FileExists} "$INSTDIR\yt-dlp.exe"
    !insertmacro CheckIfAppIsRunning "$INSTDIR\yt-dlp.exe" "${PRODUCTNAME}"
  ${EndIf}

  ; Copy main executable
  File "${MAINBINARYSRCPATH}"

  ; Copy resources
  {{#each resources_dirs}}
    CreateDirectory "$INSTDIR\\{{this}}"
  {{/each}}
  {{#each resources}}
    File /a "/oname={{this.[1]}}" "{{no-escape @key}}"
  {{/each}}

  ; Copy external binaries
  {{#each binaries}}
    File /a "/oname={{this}}" "{{no-escape @key}}"
  {{/each}}

  ; Create file associations
  {{#each file_associations as |association| ~}}
    {{#each association.ext as |ext| ~}}
       !insertmacro APP_ASSOCIATE "{{ext}}" "{{or association.name ext}}" "{{association-description association.description ext}}" "$INSTDIR\${MAINBINARYNAME}.exe,0" "Open with ${PRODUCTNAME}" "$INSTDIR\${MAINBINARYNAME}.exe $\"%1$\""
    {{/each}}
  {{/each}}

  ; Register deep links
  {{#each deep_link_protocols as |protocol| ~}}
    WriteRegStr SHCTX "Software\Classes\\{{protocol}}" "URL Protocol" ""
    WriteRegStr SHCTX "Software\Classes\\{{protocol}}" "" "URL:${BUNDLEID} protocol"
    WriteRegStr SHCTX "Software\Classes\\{{protocol}}\DefaultIcon" "" "$\"$INSTDIR\${MAINBINARYNAME}.exe$\",0"
    WriteRegStr SHCTX "Software\Classes\\{{protocol}}\shell\open\command" "" "$\"$INSTDIR\${MAINBINARYNAME}.exe$\" $\"%1$\""
  {{/each}}

  ; Create uninstaller
  WriteUninstaller "$INSTDIR\uninstall.exe"

  ; Save $INSTDIR in registry for future installations
  WriteRegStr SHCTX "${MANUPRODUCTKEY}" "" $INSTDIR

  !if "${INSTALLMODE}" == "both"
    ; Save install mode to be selected by default for the next installation such as updating
    ; or when uninstalling
    WriteRegStr SHCTX "${UNINSTKEY}" $MultiUser.InstallMode 1
  !endif

  ; Remove old main binary if it doesn't match new main binary name
  ReadRegStr $OldMainBinaryName SHCTX "${UNINSTKEY}" "MainBinaryName"
  ${If} $OldMainBinaryName != ""
  ${AndIf} $OldMainBinaryName != "${MAINBINARYNAME}.exe"
    Delete "$INSTDIR\$OldMainBinaryName"
  ${EndIf}

  ; Save current MAINBINARYNAME for future updates
  WriteRegStr SHCTX "${UNINSTKEY}" "MainBinaryName" "${MAINBINARYNAME}.exe"

  ; Registry information for add/remove programs
  WriteRegStr SHCTX "${UNINSTKEY}" "DisplayName" "${PRODUCTNAME}"
  WriteRegStr SHCTX "${UNINSTKEY}" "DisplayIcon" "$\"$INSTDIR\${MAINBINARYNAME}.exe$\""
  WriteRegStr SHCTX "${UNINSTKEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr SHCTX "${UNINSTKEY}" "Publisher" "${MANUFACTURER}"
  WriteRegStr SHCTX "${UNINSTKEY}" "InstallLocation" "$\"$INSTDIR$\""
  WriteRegStr SHCTX "${UNINSTKEY}" "UninstallString" "$\"$INSTDIR\uninstall.exe$\""
  WriteRegDWORD SHCTX "${UNINSTKEY}" "NoModify" "1"
  WriteRegDWORD SHCTX "${UNINSTKEY}" "NoRepair" "1"

  ${GetSize} "$INSTDIR" "/M=uninstall.exe /S=0K /G=0" $0 $1 $2
  IntOp $0 $0 + ${ESTIMATEDSIZE}
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD SHCTX "${UNINSTKEY}" "EstimatedSize" "$0"

  !if "${HOMEPAGE}" != ""
    WriteRegStr SHCTX "${UNINSTKEY}" "URLInfoAbout" "${HOMEPAGE}"
    WriteRegStr SHCTX "${UNINSTKEY}" "URLUpdateInfo" "${HOMEPAGE}"
    WriteRegStr SHCTX "${UNINSTKEY}" "HelpLink" "${HOMEPAGE}"
  !endif

  ; Create start menu shortcut
  !insertmacro MUI_STARTMENU_WRITE_BEGIN Application
    Call CreateOrUpdateStartMenuShortcut
  !insertmacro MUI_STARTMENU_WRITE_END

  !ifdef MDL_UPDATER
    ; MDL: update : raccourci Bureau mis à jour s'il existe (et pointe vers Ursa), jamais créé,
    ; MDL: quel que soit le mode (pas de case sur la page de fin)
    Call MdlUpdateDesktopShortcut
  !else
  ; Create desktop shortcut for silent and passive installers
  ; because finish page will be skipped
  ${If} $PassiveMode = 1
  ${OrIf} ${Silent}
    Call CreateOrUpdateDesktopShortcut
  ${EndIf}
  !endif

  !ifmacrodef NSIS_HOOK_POSTINSTALL
    !insertmacro NSIS_HOOK_POSTINSTALL
  !endif

  !ifdef MDL_UPDATER
    ; MDL: update en mode graphique : UN seul lancement de la nouvelle version, ici, une fois tous
    ; MDL: les fichiers copiés (fil de la section : la fenêtre reste réactive même si l'antivirus
    ; MDL: retarde le démarrage), puis fermeture automatique (page de fin sautée). En passif ou
    ; MDL: silencieux, rien ici : .onInstSuccess garde la convention /R du template.
    ${IfNot} ${Silent}
    ${AndIf} $PassiveMode <> 1
      DetailPrint "$(mdlUpdateLaunching)"
      ClearErrors
      !insertmacro MDL_RUN_APP
      ${IfNot} ${Errors}
        StrCpy $MdlLaunched 1
        SetAutoClose true
      ${EndIf}
    ${EndIf}
  !endif

  ; Auto close this page for passive mode
  ${If} $PassiveMode = 1
    SetAutoClose true
  ${EndIf}
SectionEnd

Function .onInstSuccess
  ; Check for `/R` flag only in silent and passive installers because
  ; GUI installer has a toggle for the user to (re)start the app
  ${If} $PassiveMode = 1
  ${OrIf} ${Silent}
    ${GetOptions} $CMDLINE "/R" $R0
    ${IfNot} ${Errors}
      ${GetOptions} $CMDLINE "/ARGS" $R0
      nsis_tauri_utils::RunAsUser "$INSTDIR\${MAINBINARYNAME}.exe" "$R0"
    ${EndIf}
  ${EndIf}
FunctionEnd

Function un.onInit
  !insertmacro SetContext

  !if "${INSTALLMODE}" == "both"
    !insertmacro MULTIUSER_UNINIT
  !endif

  !insertmacro MUI_UNGETLANGUAGE

  ${GetOptions} $CMDLINE "/P" $PassiveMode
  ${IfNot} ${Errors}
    StrCpy $PassiveMode 1
  ${EndIf}

  ${GetOptions} $CMDLINE "/UPDATE" $UpdateMode
  ${IfNot} ${Errors}
    StrCpy $UpdateMode 1
  ${EndIf}
FunctionEnd

Section Uninstall

  !ifmacrodef NSIS_HOOK_PREUNINSTALL
    !insertmacro NSIS_HOOK_PREUNINSTALL
  !endif

  !insertmacro CheckIfAppIsRunning "$INSTDIR\${MAINBINARYNAME}.exe" "${PRODUCTNAME}"

  ; Delete the app directory and its content from disk
  ; Copy main executable
  Delete "$INSTDIR\${MAINBINARYNAME}.exe"

  ; Delete resources
  {{#each resources}}
    Delete "$INSTDIR\\{{this.[1]}}"
  {{/each}}

  ; Delete external binaries
  {{#each binaries}}
    Delete "$INSTDIR\\{{this}}"
  {{/each}}

  ; Delete app associations
  {{#each file_associations as |association| ~}}
    {{#each association.ext as |ext| ~}}
      !insertmacro APP_UNASSOCIATE "{{ext}}" "{{or association.name ext}}"
    {{/each}}
  {{/each}}

  ; Delete deep links
  {{#each deep_link_protocols as |protocol| ~}}
    ReadRegStr $R7 SHCTX "Software\Classes\\{{protocol}}\shell\open\command" ""
    ${If} $R7 == "$\"$INSTDIR\${MAINBINARYNAME}.exe$\" $\"%1$\""
      DeleteRegKey SHCTX "Software\Classes\\{{protocol}}"
    ${EndIf}
  {{/each}}


  ; Delete uninstaller
  Delete "$INSTDIR\uninstall.exe"

  {{#each resources_ancestors}}
  RMDir /REBOOTOK "$INSTDIR\\{{this}}"
  {{/each}}
  RMDir "$INSTDIR"

  ; Remove shortcuts if not updating
  ${If} $UpdateMode <> 1
    !insertmacro DeleteAppUserModelId

    ; Remove start menu shortcut
    !insertmacro MUI_STARTMENU_GETFOLDER Application $AppStartMenuFolder
    !insertmacro IsShortcutTarget "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
    Pop $0
    ${If} $0 = 1
      !insertmacro UnpinShortcut "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME}.lnk"
      Delete "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME}.lnk"
      RMDir "$SMPROGRAMS\$AppStartMenuFolder"
    ${EndIf}
    !insertmacro IsShortcutTarget "$SMPROGRAMS\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
    Pop $0
    ${If} $0 = 1
      !insertmacro UnpinShortcut "$SMPROGRAMS\${PRODUCTNAME}.lnk"
      Delete "$SMPROGRAMS\${PRODUCTNAME}.lnk"
    ${EndIf}

    ; Remove desktop shortcuts
    !insertmacro IsShortcutTarget "$DESKTOP\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
    Pop $0
    ${If} $0 = 1
      !insertmacro UnpinShortcut "$DESKTOP\${PRODUCTNAME}.lnk"
      Delete "$DESKTOP\${PRODUCTNAME}.lnk"
    ${EndIf}
  ${EndIf}

  ; Remove registry information for add/remove programs
  !if "${INSTALLMODE}" == "both"
    DeleteRegKey SHCTX "${UNINSTKEY}"
  !else if "${INSTALLMODE}" == "perMachine"
    DeleteRegKey HKLM "${UNINSTKEY}"
  !else
    DeleteRegKey HKCU "${UNINSTKEY}"
  !endif

  ; Removes the Autostart entry for ${PRODUCTNAME} from the HKCU Run key if it exists.
  ; This ensures the program does not launch automatically after uninstallation if it exists.
  ; If it doesn't exist, it does nothing.
  ; We do this when not updating (to preserve the registry value on updates)
  ${If} $UpdateMode <> 1
    DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "${PRODUCTNAME}"
  ${EndIf}

  ; Delete app data if the checkbox is selected
  ; and if not updating
  ${If} $DeleteAppDataCheckboxState = 1
  ${AndIf} $UpdateMode <> 1
    ; Clear the install location $INSTDIR from registry
    DeleteRegKey SHCTX "${MANUPRODUCTKEY}"
    DeleteRegKey /ifempty SHCTX "${MANUKEY}"

    ; Clear the install language from registry
    DeleteRegValue HKCU "${MANUPRODUCTKEY}" "Installer Language"
    DeleteRegKey /ifempty HKCU "${MANUPRODUCTKEY}"
    DeleteRegKey /ifempty HKCU "${MANUKEY}"

    SetShellVarContext current
    RmDir /r "$APPDATA\${BUNDLEID}"
    RmDir /r "$LOCALAPPDATA\${BUNDLEID}"
  ${EndIf}

  !ifmacrodef NSIS_HOOK_POSTUNINSTALL
    !insertmacro NSIS_HOOK_POSTUNINSTALL
  !endif

  ; Auto close if passive mode or updating
  ${If} $PassiveMode = 1
  ${OrIf} $UpdateMode = 1
    SetAutoClose true
  ${EndIf}
SectionEnd

Function RestorePreviousInstallLocation
  ReadRegStr $4 SHCTX "${MANUPRODUCTKEY}" ""
  !insertmacro MDL_UNQUOTE $4 ; MDL: chemin du registre éventuellement entre guillemets
  StrCmp $4 "" +2 0
    StrCpy $INSTDIR $4
FunctionEnd

Function Skip
  Abort
FunctionEnd

Function SkipIfPassive
  ${IfThen} $PassiveMode = 1  ${|} Abort ${|}
FunctionEnd
Function un.SkipIfPassive
  ${IfThen} $PassiveMode = 1  ${|} Abort ${|}
FunctionEnd

Function CreateOrUpdateStartMenuShortcut
  ; We used to use product name as MAINBINARYNAME
  ; migrate old shortcuts to target the new MAINBINARYNAME
  StrCpy $R0 0

  !insertmacro IsShortcutTarget "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME}.lnk" "$INSTDIR\$OldMainBinaryName"
  Pop $0
  ${If} $0 = 1
    !insertmacro SetShortcutTarget "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
    StrCpy $R0 1
  ${EndIf}

  !insertmacro IsShortcutTarget "$SMPROGRAMS\${PRODUCTNAME}.lnk" "$INSTDIR\$OldMainBinaryName"
  Pop $0
  ${If} $0 = 1
    !insertmacro SetShortcutTarget "$SMPROGRAMS\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
    StrCpy $R0 1
  ${EndIf}

  ${If} $R0 = 1
    Return
  ${EndIf}

  ; Skip creating shortcut if in update mode or no shortcut mode
  ; but always create if migrating from wix
  ${If} $WixMode = 0
    ${If} $UpdateMode = 1
    ${OrIf} $NoShortcutMode = 1
      Return
    ${EndIf}
  ${EndIf}

  !if "${STARTMENUFOLDER}" != ""
    CreateDirectory "$SMPROGRAMS\$AppStartMenuFolder"
    CreateShortcut "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
    !insertmacro SetLnkAppUserModelId "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME}.lnk"
  !else
    CreateShortcut "$SMPROGRAMS\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
    !insertmacro SetLnkAppUserModelId "$SMPROGRAMS\${PRODUCTNAME}.lnk"
  !endif
FunctionEnd

Function CreateOrUpdateDesktopShortcut
  ; We used to use product name as MAINBINARYNAME
  ; migrate old shortcuts to target the new MAINBINARYNAME
  !insertmacro IsShortcutTarget "$DESKTOP\${PRODUCTNAME}.lnk" "$INSTDIR\$OldMainBinaryName"
  Pop $0
  ${If} $0 = 1
    !insertmacro SetShortcutTarget "$DESKTOP\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
    Return
  ${EndIf}

  ; Skip creating shortcut if in update mode or no shortcut mode
  ; but always create if migrating from wix
  ${If} $WixMode = 0
    ${If} $UpdateMode = 1
    ${OrIf} $NoShortcutMode = 1
      Return
    ${EndIf}
  ${EndIf}

  CreateShortcut "$DESKTOP\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
  !insertmacro SetLnkAppUserModelId "$DESKTOP\${PRODUCTNAME}.lnk"
FunctionEnd

; ---------------------------------------------------------------------------------------------
; MDL: fonctions ajoutées (détection d'une installation existante, raccourci Bureau de l'update)
; ---------------------------------------------------------------------------------------------

; MDL: détection (dans .onInit, après SetContext) d'une installation existante. Sorties :
; MDL:   $MdlInstalled        = 1 si ${UNINSTKEY} a un UninstallString ou un DisplayVersion, OU si
; MDL:                              le dossier enregistré existe et contient un fichier de Ursa
; MDL:                              (${MAINBINARYNAME}.exe ou uninstall.exe) ; sinon 0.
; MDL:                              Un dossier enregistré mais vide ou absent (reste d'une ancienne
; MDL:                              désinstallation, qui ne supprime pas toujours ${MANUPRODUCTKEY})
; MDL:                              ne compte pas : le setup reste alors utilisable.
; MDL:   $MdlInstalledVersion = DisplayVersion (vide si inconnue) ;
; MDL:   $MdlInstalledDir     = valeur par défaut de ${MANUPRODUCTKEY} (comme
; MDL:                              RestorePreviousInstallLocation), sinon InstallLocation de
; MDL:                              ${UNINSTKEY}, sinon dossier de UninstallString ; sans guillemets
; MDL:                              (MDL_UNQUOTE) ni « \ » final.
; MDL: Registres préservés ($0, $1).
; MDL: Lit SHCTX, c'est-à-dire HKCU en installMode currentUser (installation de l'utilisateur courant).
Function MdlDetectInstall
  Push $0
  Push $1
  StrCpy $MdlInstalled 0

  ReadRegStr $MdlInstalledVersion SHCTX "${UNINSTKEY}" "DisplayVersion"
  ReadRegStr $0 SHCTX "${UNINSTKEY}" "UninstallString"
  ${If} $0 != ""
  ${OrIf} $MdlInstalledVersion != ""
    StrCpy $MdlInstalled 1
  ${EndIf}

  ReadRegStr $1 SHCTX "${MANUPRODUCTKEY}" ""
  !insertmacro MDL_UNQUOTE $1
  ${If} $1 == ""
    ReadRegStr $1 SHCTX "${UNINSTKEY}" "InstallLocation"
    !insertmacro MDL_UNQUOTE $1
  ${EndIf}
  ${If} $1 == ""
  ${AndIf} $0 != ""
    ; UninstallString est écrit "C:\...\uninstall.exe" (avec guillemets)
    !insertmacro MDL_UNQUOTE $0
    ${GetParent} $0 $1
  ${EndIf}
  !insertmacro MDL_STRIP_TRAILING_SLASH $1
  StrCpy $MdlInstalledDir $1

  ${If} $1 != ""
    ${If} ${FileExists} "$1\${MAINBINARYNAME}.exe"
    ${OrIf} ${FileExists} "$1\uninstall.exe"
      StrCpy $MdlInstalled 1
    ${EndIf}
  ${EndIf}

  Pop $1
  Pop $0
FunctionEnd

!ifdef MDL_UPDATER
; MDL: update : met à jour le raccourci Bureau (nouvelle cible, icône, AppUserModelId) seulement
; MDL: s'il existe et pointe vers ${MAINBINARYNAME}.exe (ou vers l'ancien nom d'exe) de $INSTDIR.
; MDL: N'en crée jamais. CreateOrUpdateDesktopShortcut (template) migre l'ancien nom d'exe, sinon
; MDL: réécrit le raccourci (sauf /UPDATE ou /NS). $0-$3 modifiés (IsShortcutTarget), comme le template.
Function MdlUpdateDesktopShortcut
  !insertmacro IsShortcutTarget "$DESKTOP\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
  Pop $0
  ${If} $0 <> 1
  ${AndIf} $OldMainBinaryName != ""
    !insertmacro IsShortcutTarget "$DESKTOP\${PRODUCTNAME}.lnk" "$INSTDIR\$OldMainBinaryName"
    Pop $0
  ${EndIf}
  ${If} $0 = 1
    Call CreateOrUpdateDesktopShortcut
  ${EndIf}
FunctionEnd
!endif
