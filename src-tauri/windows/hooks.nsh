; Hooks NSIS de l'installateur Ursa (bundle.windows.nsis.installerHooks).
;
; Données utilisateur :
;   %APPDATA%\com.ursa.app       (base ursa.db, réglages, dossier de travail)
;   %LOCALAPPDATA%\com.ursa.app  (cache WebView2)
; Les vidéos téléchargées (dossier Vidéos\Ursa par défaut) ne sont JAMAIS supprimés.
;
; DESINSTALLATION (Paramètres Windows) : le désinstallateur du template supprime les données de
; l'application seulement si la case « supprimer les données de l'application » est cochée, et jamais
; avec /UPDATE ($UpdateMode = 1). Aucun hook supplémentaire n'est donc nécessaire.
;
; Mises à jour : notre template (windows/installer.nsi) ne lance AUCUN désinstallateur. Le setup
; (Ursa-Setup.exe) refuse de s'exécuter si Ursa est déjà installé ; l'exe de
; mise à jour (Ursa-Update.exe, compilé avec /DMDL_UPDATER par scripts/build-updater.mjs)
; copie les fichiers par-dessus l'installation existante. Les données ne sont jamais touchées.
;
; Ne pas renommer /UPDATE : c'est le seul drapeau que les désinstallateurs déjà installés connaissent.

; Page de fin : la case « Lancer Ursa » est COCHÉE par défaut (setup et update) ;
; MUI_FINISHPAGE_RUN_NOTCHECKED n'est donc défini nulle part. L'update lance lui-même la nouvelle
; version à la fin de la copie et saute sa page de fin, qui n'apparaît (avec cette case) que si ce
; lancement a échoué (voir MdlUpdateFinishPre dans installer.nsi).

; Case « Créer un raccourci sur le Bureau » décochée par défaut (évite un blocage sur « Terminer »
; pendant la création du raccourci / l'analyse antivirus) ; l'utilisateur peut la cocher.
; Cette case n'existe que dans le setup ; l'exe de mise à jour n'en a pas (sans effet pour lui).
!define MUI_FINISHPAGE_SHOWREADME_NOTCHECKED
