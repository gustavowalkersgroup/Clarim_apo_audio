# Clarim: notas de implementação do equalizador (Equalizer APO)

Fontes: referência de configuração https://sourceforge.net/p/equalizerapo/wiki/Configuration%20reference/ ; documentação https://sourceforge.net/p/equalizerapo/wiki/Documentation/ ; notas técnicas do dechamps/APO https://github.com/dechamps/APO ; tickets https://sourceforge.net/p/equalizerapo/tickets/ (196, 193, 208, 247, 259, 274, 281, 332). Tudo marcado como [não verificado] ou [palpite] não foi confirmado em fonte oficial.

**Estado da implementação (o que o Clarim realmente faz; vale sobre as recomendações abaixo).** O arquivo é gerado só por `clarim_core::render_eqapo_file` (SPEC, "Output equalizer (Equalizer APO)"): `Device: {GUID}` (GUID com chaves, minúsculo, como o Windows o informa), `Preamp: -X.X dB`, e os filtros `LSC Fc 105 Hz Q 0.71`, `PK Fc 1000 Hz Q 1.0`, `HSC Fc 8000 Hz Q 0.71`, com `Device: all` no fim. O arquivo fica em `config\clarim-eq.txt` na pasta do Equalizer APO, com a linha `Include: clarim-eq.txt` (caminho relativo) no fim do `config.txt` e cópia do original em `config.txt.clarim-backup`: não há `Include` para `ProgramData`. Se o `config.txt` é exatamente o padrão do instalador do APO 1.4.2 (`Preamp: -6 dB`, `Include: example.txt`, `GraphicEQ` plano de 15 bandas, o que baixa o volume de todo aparelho em 6 dB e liga um reforço de graves), o Clarim o substitui só pela linha `Include` (`clarim_core::is_stock_apo_config`; a cópia `.clarim-backup` guarda o original), ao ligar o APO num aparelho, ao terminar de instalá-lo e ao gravar o equalizador; qualquer `config.txt` editado pelo usuário só ganha a linha `Include`. Quem grava é o serviço do Clarim pelo pipe `\\.\pipe\ClarimEqApo` (ou o agente direto, se a pasta aceitar escrita). **Desde a integração (SPEC, "Equalizer APO management") o próprio serviço também instala o Equalizer APO, liga e desliga o APO em cada aparelho (sem o DeviceSelector) e reinicia o Windows Audio** a pedido do painel: a seção 0 abaixo é o mapeamento do que ele escreve no registro, com as fontes, e as seções 1 e 6 que mandam o usuário rodar o instalador, o Configurator ou o DeviceSelector ficam só como histórico da pesquisa. As presets são Neutro 0/0/0, Mais graves +5/0/0, Clareza -1/+2/+3, Agudos 0/0/+4. Continuam NÃO verificados, para conferir no primeiro teste ao vivo: o `Device:` com GUID entre chaves, a resolução do `Include` relativo, o recarregamento ao vivo depois de gravar (tmp + rename) e, de tudo o que o Clarim escreve sozinho (registro, instalador, reinício do `Audiosrv`), se o áudio fica funcionando: foi testado só contra simulações e contra os registros lidos de um PC em que o DeviceSelector rodou.

## 0) Mapeamento do DeviceSelector 1.4.2 que o Clarim implementa

Fontes: código-fonte oficial `EqualizerAPO-src-1.4.2.zip` (SourceForge, a mesma versão instalada no PC de desenvolvimento), arquivo `DeviceAPOInfo.cpp` (`load` na linha 179, `install` na 478, `uninstall` na 615), `RegistryHelper.h/.cpp`, `DeviceSelector.cpp`, `DeviceTestThread.cpp`, `EqualizerAPO/DllMain.cpp`, `EqualizerAPO.cpp`, `Setup/Setup.nsi`. Mais um PC real (Windows 11 build 22631, Equalizer APO 1.4.2, DeviceSelector rodado pelo usuário em 3 aparelhos de saída, só leitura). Implementação: `crates/clarim-svc/src/apo_plan.rs` (planejador puro, com testes que reproduzem os dumps abaixo), `apo_reg.rs` (registro), `apo_installer.rs`, `audio_restart.rs`, `apo_service.rs`.

**Chaves e valores (HKLM)**

| O quê | Onde | Fonte |
|---|---|---|
| Efeitos do aparelho | `SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render\{guid}\FxProperties`: `{d04e05a6-594b-4fb6-a80d-01af5eed7d1d},N` com N = 1 (LFX), 2 (GFX), 5 (SFX), 6 (MFX), 7 (EFX), todos REG_SZ com um CLSID; 13/14/15 (multi-APO) só são lidos para escolher o modo, nunca escritos | `DeviceAPOInfo.cpp` 52-78 |
| Modos de processamento | `{d3993a3f-99c2-4402-b5ec-a92a0367664b},5/6/7` (REG_MULTI_SZ) = `{C18E2F7E-933D-4965-B7D1-1EEF228D2AF3}`, escritos só se ausentes | idem, 559-606 |
| Título do efeito | `{b725f130-47ef-101a-a5f1-02608c9eebac},10` (só escrito quando a chave `FxProperties` é criada: "Equalizer APO") | idem |
| Melhorias de áudio desligadas | `{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},5` (DWORD 1): o DeviceSelector lê e **apaga** em `FxProperties` ao instalar; o botão do Windows grava em `Properties` | idem 611-612; Microsoft Learn (`PKEY_AudioEndpoint_Disable_SysFx`) |
| Aparelho combinado (Bluetooth no Windows 11) | `Properties`: `{b3f8fa53-0004-438e-9003-51a46e139bfc},41` | idem 377-393 |
| Registro do APO | `SOFTWARE\EqualizerAPO\Child APOs\{guid}` | idem 35-42 |
| Proteção do áudio | `SOFTWARE\Microsoft\Windows\CurrentVersion\Audio\DisableProtectedAudioDG` (DWORD 1): o DeviceSelector exige e grava; o `Setup.nsi` também grava na instalação e apaga na desinstalação | `DeviceAPOInfo.cpp` 138-177; `Setup.nsi` 279, 281, 332 |
| Classes COM | `{EACD2258-FCAC-4FF4-B36D-419E924A6D79}` = "EqualizerAPO Pre-Mix Class", `{EC1CC9CE-FAED-4822-828A-82A81A6F018F}` = "EqualizerAPO Post-Mix Class", ambas `InprocServer32` = `EqualizerAPO.dll` (registradas pelo `regsvr32 /s` do instalador) | `RegistryHelper.h` 29 e 31; `DllMain.cpp` 65-134 |
| Instalação | `SOFTWARE\EqualizerAPO`: `InstallPath`, `ConfigPath`, `EnableTrace`=`false`, `Start Menu Folder`; chave de desinstalação `...\Uninstall\EqualizerAPO` (`DisplayVersion` 1.4.2) | PC de desenvolvimento (leitura); `Setup.nsi` |

**Registro do APO (`Child APOs\{guid}`)**: os valores `{d04e05a6-...},1`, `,2`, `,5`, `,6`, `,7` guardam o valor original do slot, `!VALUE` (o valor não existia) ou `!KEY` (a chave `FxProperties` inteira não existia); `PreMixChild` e `PostMixChild` = os CLSIDs originais para os quais o APO encaminha o áudio (vazio = nenhum; a DLL trata vazio, `{00000000-...}`, `!KEY` e `!VALUE` como "sem filho"); `AllowSilentBufferModification` = `false`; `Version` = `2` (a DLL se recusa a carregar outra versão: "Unsupported version of APO installation detected"; sem `Version` é a versão 1, lida de outro jeito); `DisableAutomaticAdjustment` só existe se a opção foi desmarcada (o Clarim não grava).

**Modo de instalação** (aparelho ainda sem APO, Windows 8.1+): só slots LFX/GFX e nenhum SFX/MFX/EFX/multi = `LFX_GFX`; senão `Properties ,41` presente = `SFX_MFX`; senão `SFX_EFX` (o padrão: o que Realtek, USB e Bluetooth receberam no PC de desenvolvimento). Instalado, o modo é deduzido de qual slot aponta para o APO.

**Ordem do `install`** (`DeviceAPOInfo.cpp` 483-613): (1) cria `Child APOs\{guid}`; (2) se `FxProperties` não existe, cria (título "Equalizer APO") e grava os 5 valores do registro como `!KEY`; senão grava, para cada slot, o valor atual ou `!VALUE`, e guarda um `backup_<aparelho>_<conexão>.reg` com os valores presentes; (3) `PreMixChild` / `PostMixChild`; (4) `AllowSilentBufferModification`; (5) `Version`=`2`; (6) escritas do modo: `SFX_EFX` apaga `,1` e `,2`, grava `,5`=Pre-Mix (+ modo de processamento `,5` se ausente) e `,7`=Post-Mix (+ modo `,7` se ausente), não toca em `,6`; `SFX_MFX` grava `,5` e `,6`; `LFX_GFX` grava `,1` e `,2` e apaga `,5 ,6 ,7`; (7) apaga `Disable_SysFx` se existir. **`uninstall`** (615-651): se o slot 1 do registro é `!KEY`, apaga a chave `FxProperties` inteira; senão, para cada slot, `!VALUE` apaga o valor, vazio ignora, qualquer outro valor é gravado de volta; apaga `Child APOs\{guid}` e a chave `Child APOs` se ficou vazia. O `uninstall` do DeviceSelector **não** restaura `Disable_SysFx` e **não** remove os modos de processamento `{d3993a3f-...}`: por isso o Clarim guarda `ClarimCreatedModes` e `ClarimSysFxWas` no registro do APO (o APO ignora valores desconhecidos) e os desfaz, para que ligar e desligar devolva o estado exato.

**Verificado contra o PC de desenvolvimento (Realtek `{171dffe0-1f81-4558-a253-45945204afa2}`, USB `{9fc5e635-237f-4f55-add4-030c65c6a5c9}`, Bluetooth WZ-BT5.0 `{b9cc1e4e-11bd-4f54-8227-5e28f30a485d}`)**: antes, `,5` = `{62dc1a93-ae24-464c-a43e-452f824c4250}` e `,6` = `{637c490d-eee3-4c0a-973f-371958802da2}` (Realtek e USB; Bluetooth `,5` = `{C9453E73-8C5C-4463-9984-AF8BAB2F5447}`, `,6` = `{13AB3EBD-137E-4903-9D89-60BE8277FD17}`); depois, `,5` = Pre-Mix, `,6` igual, `,7` = Post-Mix (não existia), `{d3993a3f},5/6/7` = `{C18E2F7E-...}`, e o registro com `,1 ,2 ,7` = `!VALUE`, `,5` e `,6` com os originais, `PreMixChild` = o `,5` original, `PostMixChild` vazio, `AllowSilentBufferModification` = `false`, `Version` = `2`. Os testes de `apo_plan.rs` reproduzem exatamente esses antes e depois. Pontos que o código-fonte não explica e que o Clarim não afirma: `{d3993a3f},6` já existir no Realtek (vem do Windows ou do driver, o modo `SFX_EFX` não o escreve), e o `PostMixChild` do Bluetooth.

**Instalador** (fixado em `apo_installer.rs`): `EqualizerAPO-x64-1.4.2.exe` (12,0 MB, SourceForge), SHA-256 `7403be7427bbe1936a40dded082829b6e217fc4f5990fee5cba501f0ae055afa` (do `checksum64` do pacote Chocolatey da 1.4.2, que aponta para a mesma URL; o mesmo hash aparece num relatório público do ANY.RUN; o SourceForge não publica SHA-256 e o arquivo **não foi baixado** para conferir, então vale uma conferência única numa máquina de teste). O instalador aceita `/S` (NSIS padrão, sem tratamento próprio de modo silencioso), registra a DLL com `regsvr32 /s`, roda `DeviceSelector.exe /i` (diálogo interativo: o Clarim o encerra enquanto o instalador roda, ou ficaria invisível na sessão 0 para sempre) e `UpdateChecker.exe -i` (agenda uma verificação de atualização a cada logon: o Clarim roda `UpdateChecker.exe -u` depois, a mesma chave que o instalador usa quando a caixa de atualização é desmarcada). Ainda **não verificado**: se o instalador, sob `/S` e como SYSTEM, termina sem pedir reinício; o comportamento da desinstalação silenciosa (ticket 245).

**Efeito e reinício** (pesquisa "activation"): o DeviceSelector não reinicia o `Audiosrv` ao aplicar; é o diálogo de teste dele que reinicia (para o `Audiosrv` e os serviços que dependem dele, e inicia na ordem inversa). O Clarim faz o mesmo reinício, a pedido. O `Audiosrv` depende de `AudioEndpointBuilder` e `RpcSs`; neste PC os serviços que dependem do `Audiosrv` são `AarSvc` (parado) e `LightingService` (ASUS, rodando), que o Clarim para e volta a iniciar. `needs_restart` vem da comparação entre o início do `audiodg.exe` e a última alteração do registro: no PC de desenvolvimento o `audiodg.exe` subiu 0,55 s depois das escritas do DeviceSelector (o teste dele reiniciou o áudio), então ali `needs_restart` é falso. `apo_loaded` (lista de módulos do `audiodg.exe`) não pôde ser lido como usuário comum e ainda não foi verificado a partir do SYSTEM (se `audiodg.exe` é processo protegido com `DisableProtectedAudioDG=1` é provável que não, mas não foi testado), e mesmo quando funciona é só informação: `Some(false)` é normal enquanto nada toca num aparelho com o APO. O Windows desliga os efeitos do aparelho (`Disable_SysFx`=1) depois de 10 falhas ao carregar um APO (Microsoft Learn, "Troubleshooting APO Load Failures"): por isso o painel avisa quando as melhorias estão desligadas, sem religar sozinho. Bluetooth no Windows 11 continua sendo o ponto fraco (ticket 196 aberto desde 2022); o modo `SFX_MFX` é o padrão para aparelhos combinados.

**Segurança da escrita no registro**: a chave `FxProperties` pertence a SYSTEM; Administradores têm SetValue + ReadKey, TrustedInstaller tem controle total, Audiosrv e AudioEndpointBuilder podem alterar. O serviço do Clarim roda como LocalSystem (membro de Administradores) e por isso consegue gravar e apagar valores. A chave de um aparelho que não tem `FxProperties` e cuja criação for recusada pela ACL (o DeviceSelector tenta tomar a posse com `takeOwnership`/`makeWritable`) **não** é tratada pelo Clarim: o pedido falha com uma mensagem em PT-BR e é desfeito. Registros de `Child APOs` com `Version` 1 (legado) são recusados em vez de atualizados. Instalação do Equalizer APO fora de `<Program Files>\EqualizerAPO` e Windows que não seja x64 não são tratados.


## 1) Instalação e detecção

- Executável de configuração: `C:\Program Files\EqualizerAPO\Configurator.exe` (alta). Nenhum argumento de linha de comando é documentado; use a interface gráfica ou escreva os arquivos diretamente.
- Detecção da instalação (mais confiável que valores de registro):
  - Arquivo `C:\Program Files\EqualizerAPO\EqualizerAPO.dll` existe (alta, dechamps/APO).
  - Chave `HKLM\SOFTWARE\Classes\CLSID\{EC1CC9CE-FAED-4822-828A-82A81A6F018F}\InprocServer32`, valor padrão = caminho da DLL acima (alta, dechamps/APO). Esse é o CLSID do sAPO GFX (post-mix).
- Ativação por endpoint: `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render\{GUID}\FxProperties`. Procure o CLSID acima nos valores de `FxProperties` do endpoint para saber se o APO está instalado nele. Qual valor exato o instalador grava: [não verificado]. Verifique comparando as chaves antes e depois de marcar o dispositivo no Configurator.
- Caminho do config: `C:\Program Files\EqualizerAPO\config\config.txt` (média; fontes secundárias e a referência descrevem a pasta config sob Program Files).
- Valor de registro `ConfigPath` / `InstallPath` em `HKLM\SOFTWARE\EqualizerAPO`: **verificado** num PC com o Equalizer APO 1.4.2 instalado (existem, junto com `EnableTrace` e `Start Menu Folder`; seção 0). O Clarim ainda usa só a pasta do `EqualizerAPO.dll` e confere `InstallPath` depois de instalar.
- Versão atual: 1.4.2, instalador x64 de ~12 MB na página de Configurator do SourceForge (média) https://sourceforge.net/projects/equalizerapo/files/Configurator/ . Um espelho (deepwiki) mostra 1.4.0; prefira o SourceForge.
- Após mudanças de instalação ou de dispositivo, o áudio precisa reiniciar (serviço `audiosrv`) (alta). Reiniciar `audiosrv` exige admin: o serviço LocalSystem do Clarim faz isso pelo botão "Reiniciar áudio agora" do painel (seção 0).

## 2) Estratégia de escrita da configuração

Fatos:
- Só `config.txt` é lido como configuração principal. Outros arquivos entram por `Include: <arquivo>` dentro dele (média, ticket 208).
- Permissão de escrita para usuário padrão na pasta config: confirmada no PC de desenvolvimento, `Users` tem controle total depois do instalador (pergunta 6 da seção 7). Antes disso eram só relatos de que salvar `config.txt` exige admin às vezes, e de antivírus ou firewall bloqueando (média, ticket 208). O resto de Program Files é só de administradores; esta pasta é a exceção que o instalador abre.
- Live update: a referência diz que alterações nos arquivos da pasta de configuração recarregam automaticamente (média). O dechamps/APO diz que mover a configuração para fora da pasta de config quebra as atualizações ao vivo (média). Isso é um conflito direto com a ideia de `Include` apontando para uma pasta de usuário.
- Caminho do `Include`: a referência só diz que o caminho de Convolution é relativo ao arquivo de configuração atual. Para `Include` de arquivos comuns, a resolução de caminho não foi confirmada [não verificado]. Use caminho absoluto, sem espaços, até confirmar.

Recomendação (decisão de design, baseada nos fatos acima):
1. O serviço LocalSystem do Clarim é quem escreve na pasta do APO. O app de usuário não escreve em Program Files. O app pede as mudanças ao serviço por IPC (named pipe, por exemplo).
2. Na instalação do Clarim (com UAC), o serviço cria um `config.txt` com uma única linha, por exemplo `Include: C:\ProgramData\Clarim\eq\clarim-eq.txt`, depois de fazer backup do `config.txt` original. Esse é um arquivo que o usuário padrão pode ler, mas o serviço escreve.
3. O serviço escreve o arquivo do Clarim com escrita atômica (arquivo temporário e renomear) e depois reinicia `audiosrv` se o APO não recarregar sozinho. Se o live update não funcionar com o arquivo fora da pasta config, esse reinício é o caminho de fallback.
4. Fallback se a pasta config for gravável pelo usuário: verifique a ACL em runtime e escreva direto em `config\clarim-eq.txt` com `Include` apontando para ele. Isso mantém o live update dentro da pasta config.
5. Antes de editar, sempre faça backup de `config.txt`. Na desinstalação do Clarim, remova a linha `Include`.
6. Encoding: use UTF-8 sem BOM e teste com um nome de dispositivo com acento. Não confirmado se APO lida bem com acentos no `Device:` [não verificado].

## 3) Template de arquivo: EQ de 3 bandas por dispositivo, com preamp

Bandas (escolha do Clarim, baseada no exemplo da referência):
- Graves: `LS` (low shelf) em 100 Hz, slope 6 dB.
- Medios: `PK` (peaking) em 1000 Hz, Q 1.0.
- Agudos: `HS` (high shelf) em 8000 Hz, slope 6 dB.

Sintaxe das linhas de filtro (alta, referência):
- `Filter N: ON PK Fc <hz> Hz Gain <db> dB Q <q>`
- `Filter N: ON LS 6dB Fc <hz> Hz Gain <db> dB` e o equivalente `HS`.
- Conflito de sintaxe: alguns exemplos (ticket/snippets do relatório) mostram `ON LS Fc 300 Hz Gain 5.0 dB` sem o slope. Usar o slope explícito (`6dB`) é o mais seguro. Se o APO recusar a linha, a linha é ignorada em silêncio (alta: comandos inválidos são ignorados sem erro). Teste.
- Não use `GraphicEQ` no Bluetooth. O ticket 193 relata ruído com Graphic EQ no Bluetooth, e o mantenedor sugere filtros PK (média).

Presets (valores em dB; os de "Clareza" e "Agudos" são palpites de produto, não de fonte):
| Preset | Graves (LS 100) | Medios (PK 1k) | Agudos (HS 8k) |
|---|---|---|---|
| Neutro | 0 | 0 | 0 |
| Mais graves | +4 | 0 | 0 |
| Clareza | 0 | +2 | +3 |
| Agudos | 0 | 0 | +4 |

Preamp: `Preamp = -max(0, maior ganho positivo entre as 3 bandas)`. Exemplo: +4 dB vira `Preamp: -4 dB`. Motivo: ganhos positivos podem causar clipping, e reduzir o preamp compensa (alta, wiki Documentation). A regra exata de quanto reduzir não foi encontrada [palpite].

Template (um bloco por dispositivo, cada um começa com a linha `Device:`):

```
Device: <PADRAO_DO_DISPOSITIVO>
Preamp: -4 dB
Filter 1: ON LS 6dB Fc 100 Hz Gain 4 dB
Filter 2: ON PK Fc 1000 Hz Gain 0 dB Q 1.0
Filter 3: ON HS 6dB Fc 8000 Hz Gain 0 dB

Device: <PADRAO_DO_OUTRO_DISPOSITIVO>
Preamp: 0 dB
Filter 1: ON LS 6dB Fc 100 Hz Gain 0 dB
Filter 2: ON PK Fc 1000 Hz Gain 0 dB Q 1.0
Filter 3: ON HS 6dB Fc 8000 Hz Gain 0 dB
```

Regras do arquivo:
- Não há linha `Device: all` de "reset" no template. A regra da referência é que, se o padrão não bate, todos os comandos seguintes (exceto `Device`) são ignorados até o próximo `Device`. Como cada bloco começa com a sua própria linha `Device`, cada bloco é independente. Isso é uma inferência lógica, não verificada no texto oficial.
- Dispositivos sem EQ configurado não recebem bloco. Assim, o APO aplica só o que existe.
- O `Include` fica sozinho em `config.txt`, antes de qualquer `Device`. Nenhuma outra linha vai depois dele.
- Linhas `Device` não podem ficar dentro de blocos `If` (alta). Não use `If`.
- Evite comentários com `#` dentro dos blocos. A referência não confirma o suporte a comentários [não verificado]. Se precisar, use só nos cabeçalhos do arquivo gerado.

## 4) Regra de correspondência do dispositivo

- Padrão `Device:` (alta): é comparado com o nome do dispositivo, o nome da conexão e o GUID. Todas as palavras do padrão precisam aparecer. Alternativas são separadas por `;`. `all` sempre casa.
- Fonte da verdade: use o texto gerado pelo próprio Configurator ("Copy Device command to clipboard") como padrão de referência (alta, wiki Documentation). O Clarim deve reproduzir o mesmo formato.
- Se o Clarim gerar o padrão sozinho: use o nome amigável do endpoint de renderização (lido de MMDevices) [palpite, nome da propriedade não verificado]. Palavras genéricas como "Speakers" ou "de" casam com muitos dispositivos. Prefira o nome completo com o GUID do endpoint como alternativa (`Device: <nome completo>; <GUID>`).
- GUID com chaves, formato `{...}`: se o APO aceita o GUID com chaves ou precisa de um prefixo, não foi documentado [não verificado]. Teste com o texto gerado pelo Configurator.
- Bluetooth: o fone aparece como dois endpoints (estéreo A2DP e mãos-livres HFP). O bloco deve casar com o endpoint estéreo, que é onde o usuário toca música. Se o nome do mãos-livres contiver o mesmo nome do fone, o padrão pode casar com os dois. Verifique antes de liberar.
- GUID muda se o driver for reinstalado. Reconsulte o GUID do endpoint a cada aplicação, não guarde fixo (média, ticket 247 e notas do dechamps).
- Padrão que não casa é silencioso: o EQ fica desligado sem erro. A UI do Clarim deve mostrar "ativo" ou "inativo" por dispositivo, comparando o endpoint atual com o padrão.

## 5) Bluetooth, VB-Cable e outras armadilhas

- Bluetooth no Windows 11: o ticket 196 (aberto em 2022-01-02, sem resposta do mantenedor) relata que o APO não funciona com áudio Bluetooth no Windows 11. Trate o suporte ao WZ-BT5.0 como não garantido. Teste antes de prometer (alta, fato do ticket).
- Ticket 274: crepitação com SFX/MFX no Bluetooth. Ticket 193: ruído com Graphic EQ. Use PK/LS/HS (já é o plano).
- Modo de instalação: a documentação indica SFX/EFX primeiro, depois SFX/MFX se o primeiro não existir (média). Para o endpoint combinado Bluetooth, SFX/MFX é o padrão reportado (média). Teste no WZ-BT5.0. Os modos ficam em "Install as" nas opções de solução de problemas do Configurator.
- Melhorias de áudio do Windows: desativar "Disable all enhancements" / "Enable audio enhancements" no endpoint também desativa o APO (média). O campo do Windows é `PKEY_AudioEndpoint_Disable_SysFx` (DWORD, 1 desativa todos os sAPOs) https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/pkey-fx-endpointeffectclsid . O Clarim deve detectar isso e avisar.
- Atualizações do Windows podem quebrar a instalação. O mantenedor diz que rodar o Configurator costuma consertar (média, tickets 185, 199, 259).
- Windows 11 24H2: relatos de problemas com LFX/GFX e SFX/EFX (tickets 332, 281). Não há correção oficial encontrada. Não testado com o WZ-BT5.0.
- VB-Cable: não é necessário para EQ por dispositivo. Ele só serve para o roteamento estilo HeSuVi, e não tem controle de volume (média) https://sourceforge.net/p/hesuvi/wiki/Help/ . Não instale APO em dispositivos CABLE para o Clarim.
- Desinstalação: não há procedimento oficial limpo. Comunidade: desinstalar o Configurator, depois o APO, reiniciar, e reparar o driver de áudio se o som não voltar. Um `EqualizerAPO.log` pode sobrar em `C:\Windows\ServiceProfiles\LocalService\AppData\Local\Temp` (média). Com o Clarim: desligue a chave "Equalizador neste aparelho" de cada aparelho (o Clarim restaura o registro do aparelho) e só então desinstale o Equalizer APO em Configurações > Aplicativos; o `clarim uninstall` não faz isso.
- Licença: fontes secundárias dizem GPLv2 para o projeto principal (média). Confirme no repositório antes de empacotar binários do APO. O Clarim deve baixar o instalador do usuário, não redistribuí-lo, até confirmar.
- Custo de CPU: não encontrado.

## 6) Passos de instalação manual (histórico: o Clarim faz tudo isso pelo painel, veja a seção 0; valem só como plano B sem o serviço do Clarim)

1. Baixe o Equalizer APO no site oficial do SourceForge (https://sourceforge.net/projects/equalizerapo/). Use a versão 1.4.2 x64.
2. Execute o instalador e mantenha o caminho padrão `C:\Program Files\EqualizerAPO`.
3. Na tela do Configurator, marque o dispositivo de saída que você usa, por exemplo "Fones de ouvido (WZ-BT5.0)". Se tiver dois endpoints de fone Bluetooth, escolha o estéreo.
4. Reinicie o computador quando o instalador pedir.
5. Abra o Clarim e vá em Equalizador. Escolha o dispositivo de saída e um preset, ou ajuste os três controles (Graves, Médios, Agudos).
6. Se o efeito não aparecer ou o som ficar estranho: abra `Configurator.exe`, confira se o dispositivo está marcado e clique em "Restart Audio Service".
7. Se o Clarim avisar que as "Melhorias de áudio" estão desativadas, ative-as nas propriedades do dispositivo no Windows, ou desative a opção que o Clarim indicar.
8. Com Bluetooth, o efeito pode falhar ou crepitar. Se isso acontecer, teste com fio ou outro dispositivo, e avise no suporte.

## 7) Perguntas em aberto e conflitos resolvidos

Conflitos resolvidos:
- Nome vs GUID no `Device:`: a referência diz que casa com nome, conexão e GUID. Os relatórios concordam. Resolvido.
- Sintaxe de `LS`/`HS`: exemplos com e sem slope `6dB`. Usar o slope explícito; a forma sem slope não foi confirmada.
- Caminho do config: todos os relatórios dão Program Files. Resolvido como padrão, mas o `ConfigPath` no registro é não verificado.
- Versão: SourceForge (1.4.2) prevalece sobre o espelho (1.4.0).
- Include para fora da pasta config: a referência permite, mas o dechamps diz que quebra o live update. Conflito não resolvido. A recomendação é escrever via serviço e usar o reinício de `audiosrv` como fallback.
- Modo de instalação no Bluetooth: SFX/MFX é o padrão reportado, mas não confirmado no WZ-BT5.0.

Perguntas em aberto:
1. O `Device:` aceita o GUID com chaves? Teste com o texto gerado pelo Configurator.
2. Qual valor de `FxProperties` o instalador grava para ativar o APO? Compare as chaves antes e depois de marcar no Configurator.
3. O `Include` aceita caminho absoluto com pasta fora de Program Files? A resolução de caminho não foi documentada.
4. O live update funciona com o arquivo do Clarim fora da pasta config? Ou só depois de reiniciar `audiosrv`?
5. Quanto reduzir o preamp para evitar clipping? Só dá para medir ou ouvir.
6. (Resolvida) O instalador dá permissão de escrita a "Users" na pasta config? Sim. `icacls` no PC de desenvolvimento, com o Equalizer APO 1.4.2 instalado pelo instalador oficial: `C:\Program Files\EqualizerAPO\config` tem `BUILTIN\Users:(OI)(CI)(F)` e `config.txt` tem `BUILTIN\Users:(I)(F)`. Consequências: (a) o agente do Clarim (usuário comum) consegue escrever `clarim-eq.txt` direto, sem o serviço (escritor B); (b) qualquer conta do PC pode editar o `config.txt` (por exemplo acrescentar `VSTPlugin:`, que o motor de áudio carrega): é o desenho do próprio Equalizer APO, e o Clarim não muda essa permissão; (c) o serviço LocalSystem do Clarim mexe nessa pasta, então nunca segue links: todo arquivo dela é aberto com `FILE_FLAG_OPEN_REPARSE_POINT` (o link em si, não o destino), o `config.txt` só é lido se for um arquivo comum de até 1 MiB, a cópia `config.txt.clarim-backup` e o arquivo temporário são criados com `create_new` (um link plantado ali conta como "já existe") e o temporário é apagado antes (`remove_file` apaga o link, não o destino).
7. O serviço LocalSystem do Clarim pode escrever no caminho escolhido, incluindo `ProgramData`? Esperado que sim, mas não testado.
8. O WZ-BT5.0 funciona em SFX/EFX ou SFX/MFX no Windows 11 24H2? Não testado; só relatos de 2021 a 2022.
9. A linha `Device:` do endpoint mãos-livres também casa com o nome do fone? Verifique com o Clarim em um fone real.
10. Comentários `#` são aceitos pelo parser? Não confirmado.


---
## Raw research (Haiku)

```json
[
 {
  "facts": [
   {
    "claim": "Config lines have the form 'Command: Parameters'. Lines not in this format are silently ignored, and so are unsupported command names.",
    "source": "https://sourceforge.net/p/equalizerapo/wiki/Configuration%20reference/",
    "confidence": "high"
   },
   {
    "claim": "Include syntax is 'Include: <File name>'. The page does not state how relative paths resolve for Include; for Convolution, the file name is relative to the current configuration file's path.",
    "source": "https://sourceforge.net/p/equalizerapo/wiki/Configuration%20reference/",
    "confidence": "high"
   },
   {
    "claim": "Device syntax is 'Device: <pattern1>; <pattern2>; ...' (since 0.7). A pattern is matched against 'Device_name Connection_name GUID'; all space-separated words in a pattern must be found; one of the ';'-separated alternatives must match. The special pattern 'all' always matches. If a pattern does not match, all following commands except Device commands are ignored.",
    "source": "https://sourceforge.net/p/equalizerapo/wiki/Configuration%20reference/",
    "confidence": "high"
   },
   {
    "claim": "The page gives no explicit reset syntax for Device. Inference: because non-matching Device lines suppress following filters, a 'Device: all' line placed after a device block should re-enable filters for other devices. Not verified in the docs.",
    "source": "https://sourceforge.net/p/equalizerapo/wiki/Configuration%20reference/",
    "confidence": "low"
   },
   {
    "claim": "Preamp syntax is 'Preamp: <number> dB' (example 'Preamp: -6.5 dB'). Since 0.8 multiple preamps on the same channel are summed in dB.",
    "source": "https://sourceforge.net/p/equalizerapo/wiki/Configuration%20reference/",
    "confidence": "high"
   },
   {
    "claim": "Filter syntax: 'Filter <n>: ON <Type> Fc <freq> Hz Gain <gain> dB Q <q>' or '... BW Oct <bw>'. The number n is not interpreted and can be omitted. PK requires Fc, Gain and Q/BW, e.g. 'ON PK Fc 50.0 Hz Gain -10.0 dB Q 2.50'. LS/HS take Fc and Gain with 6 or 12 dB per octave, e.g. 'ON LS 6dB Fc 50.0 Hz Gain 7.2 dB'. LSC/HSC take a slope in 'x dB' after the type, with Q optional since 1.2.1, e.g. 'ON LSC 10.8 dB Fc 300 Hz Gain 5.0 dB' and 'ON HSC Fc 100 Hz Gain -6.0 dB Q 0.4272'.",
    "source": "https://sourceforge.net/p/equalizerapo/wiki/Configuration%20reference/",
    "confidence": "high"
   },
   {
    "claim": "Channel syntax is 'Channel: <positions>' (since 0.8). It selects channels for the following Filter and Preamp lines. Use 'all' for every channel. Stage syntax is 'Stage: pre-mix | post-mix | capture' (since 0.9).",
    "source": "https://sourceforge.net/p/equalizerapo/wiki/Configuration%20reference/",
    "confidence": "high"
   },
   {
    "claim": "Auto reload: the reference says changes to Convolution files in the config path or a subdirectory reload automatically, and registry values read via readRegString/readRegDWORD reload on change. A separate search result says that if files in the config path change, the configuration is reloaded automatically, and that saving config.txt takes effect without a restart. Exact latency was not found.",
    "source": "https://sourceforge.net/p/equalizerapo/wiki/Configuration%20reference/",
    "confidence": "medium"
   },
   {
    "claim": "Only config.txt is read as the main config. Other files must be pulled in via an Include line inside config.txt.",
    "source": "https://sourceforge.net/p/equalizerapo/tickets/208",
    "confidence": "medium"
   },
   {
    "claim": "Default config path is under the install folder, C:\\Program Files\\EqualizerAPO\\config\\config.txt (per the 'config folder under Program Files' description). The reference does not state the default path directly; it only shows a ConfigPath registry read in an example.",
    "source": "https://www.itechguides.com/?p=331240",
    "confidence": "medium"
   },
   {
    "claim": "Non-admin write access to the config folder is not confirmed. Users report that saving config.txt sometimes requires administrator rights, and that antivirus or firewall apps can block access to the APO folder and config.txt. An Appuals article says the most common cause of failures is installation without sufficient permissions. No source confirms the installer grants Users Modify rights. Verification would need a check of the folder's Security tab on a real install.",
    "source": "https://sourceforge.net/p/equalizerapo/tickets/208",
    "confidence": "medium"
   },
   {
    "claim": "The docs describe no ACL configuration. The practical options are: grant Users Modify on the config folder (a guess, unverified), or keep the real config in a user-writable folder and reference it via Include from config.txt (also unverified for permissions, but Include is documented). The Include target path resolution still needs checking.",
    "source": "https://sourceforge.net/p/equalizerapo/wiki/Configuration%20reference/",
    "confidence": "low"
   }
  ],
  "snippets": [
   "Device: Fones de ouvido (WZ-BT5.0)\nPreamp: -4 dB\nFilter 1: ON LSC 6 dB Fc 100 Hz Gain 3 dB\nFilter 2: ON PK Fc 1000 Hz Gain 0 dB Q 1.0\nFilter 3: ON HSC 6 dB Fc 8000 Hz Gain 2 dB\n\nDevice: USB Speakers\nPreamp: -3 dB\nFilter 1: ON LSC 6 dB Fc 100 Hz Gain -2 dB\n\nDevice: all\n# no further filters apply to other devices (inferred, not verified in docs)",
   "Filter 1: ON PK Fc 50.0 Hz Gain -10.0 dB Q 2.50",
   "Filter 1: ON LSC 10.8 dB Fc 300 Hz Gain 5.0 dB",
   "Filter 1: ON HSC Fc 100 Hz Gain -6.0 dB Q 0.4272",
   "Include: example.txt",
   "Channel: L R\nPreamp: -6.5 dB"
  ],
  "gotchas": [
   "Device patterns are matched as space-separated word sets against 'Device_name Connection_name GUID', so parenthesised names and multi-word names need no escaping, but a short pattern can match more devices than intended.",
   "A non-matching Device line silently suppresses all following non-Device commands until the next matching Device line or 'Device: all'. Typos in device names fail silently.",
   "Unsupported or mistyped command names are silently ignored, so bad syntax produces no error.",
   "Device lines cannot be placed inside If conditionals (they have higher priority).",
   "Preamp values from multiple lines are summed in dB, so stacking Preamp lines compounds attenuation.",
   "Saving config.txt may fail for a standard user due to permissions or AV/firewall blocking; the Program Files location is the likely cause.",
   "Recommended preamp to avoid clipping when boosting: not found in the sources. Guess: set preamp to roughly the largest positive gain across filters in that device block (e.g. -4 dB for a +4 dB boost). Verify by listening/meter."
  ],
  "open_questions": [
   "Exact Device reset behavior: confirm that 'Device: all' restores filters for other devices (not stated in the docs).",
   "Whether a standard user can write config.txt by default on a fresh install (installer ACL on C:\\Program Files\\EqualizerAPO\\config). Needs a check of the folder's Security tab.",
   "Include relative path resolution for non-Convolution files and whether a per-user folder outside Program Files works with Include.",
   "Auto-reload latency after saving config.txt (only 'takes effect without restart' is reported).",
   "Recommended preamp for boosting: needs a source or a measured test.",
   "Whether the Clarim service (LocalSystem) can write to the config folder directly, or whether the user-level app must write it (installer and ACL question)."
  ]
 },
 {
  "facts": [
   {
    "claim": "Equalizer APO's main config file is C:\\Program Files\\EqualizerAPO\\config\\config.txt, loaded automatically. Configurator.exe is at C:\\Program Files\\EqualizerAPO\\Configurator.exe.",
    "source": "https://sourceforge.net/p/equalizerapo/wiki/Documentation/ (install/config sections); https://sourceforge.net/p/equalizerapo/wiki/Configuration%20reference/",
    "confidence": "high"
   },
   {
    "claim": "The Device: line pattern is matched against the connection name, device name and GUID of the output device. Words in a pattern must all be found; multiple patterns are separated by ';'; the special pattern 'all' always matches. If the pattern does not match, all following commands except Device commands are ignored. Example: 'Device: High Definition Audio Device Speakers; Benchmark'.",
    "source": "https://sourceforge.net/p/equalizerapo/wiki/Configuration%20reference/",
    "confidence": "high"
   },
   {
    "claim": "The wiki does not say whether to use name or GUID in the Device line. Because any word can match any of the three fields, a GUID word is a valid match token, and the wiki recommends generating the Device line with the Configurator button ('Copy Device command to clipboard') rather than writing it by hand.",
    "source": "https://sourceforge.net/p/equalizerapo/wiki/Configuration%20reference/ ; https://sourceforge.net/p/equalizerapo/wiki/Documentation/",
    "confidence": "high"
   },
   {
    "claim": "Endpoint GUIDs are braced, hyphenated, e.g. {5D16E8DA-37A3-4657-8D52-57FFE97F4EF9} for a Render endpoint, and match the key under HKLM\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\MMDevices\\Audio\\Render\\{GUID}\\FxProperties.",
    "source": "https://sourceforge.net/p/equalizerapo/wiki/Configuration%20reference/ (via search snippets); https://github.com/dechamps/APO",
    "confidence": "medium"
   },
   {
    "claim": "Equalizer APO's GFX sAPO CLSID is {EC1CC9CE-FAED-4822-828A-82A81A6F018F}. It is registered under HKLM\\SOFTWARE\\Classes\\CLSID\\{EC1CC9CE-...}, with default value 'EqualizerAPO Post-Mix Class' and InprocServer32 pointing to C:\\Program Files\\EqualizerAPO\\EqualizerAPO.dll. This is a practical detection path: check the file and the CLSID key, rather than a SOFTWARE\\EqualizerAPO value.",
    "source": "https://github.com/dechamps/APO",
    "confidence": "high"
   },
   {
    "claim": "The dechamps/APO page does not document any HKLM\\SOFTWARE\\EqualizerAPO InstallPath or ConfigPath values. Searches also did not confirm them. A SourceForge comment says the installer creates ConfigPath under the EqualizerAPO key, but this is a commenter's guess. The only documented value under HKLM\\SOFTWARE\\EqualizerAPO is EnableTrace (set to true for trace logging).",
    "source": "https://sourceforge.net/p/equalizerapo/wiki/Documentation/ ; https://sourceforge.net/p/equalizerapo/tickets/247",
    "confidence": "high"
   },
   {
    "claim": "Per-endpoint enablement lives under MMDevices\\Audio\\Render\\{GUID}\\FxProperties. The GFX (post-mix) property is listed as {d04e05a6-594b-4fb6-a80d-01af5eed7d1d},2, but the source does not say which value Equalizer APO writes for enablement. Microsoft's PKEY_AudioEndpoint_Disable_SysFx (DWORD, 1 disables all sAPOs) maps to the 'Enable audio enhancements' / 'Disable all enhancements' checkbox.",
    "source": "https://github.com/dechamps/APO ; https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/pkey-fx-endpointeffectclsid",
    "confidence": "medium"
   },
   {
    "claim": "The FxProperties approach is not officially supported by Microsoft. Driver reinstalls can overwrite the config and change endpoint GUIDs. After changing registry values, restart audiosrv.",
    "source": "https://github.com/dechamps/APO ; https://sourceforge.net/p/equalizerapo/wiki/Configuration%20reference/",
    "confidence": "high"
   },
   {
    "claim": "Installation: the installer runs Configurator.exe; the user selects the audio device to install the APO on; a reboot is needed because the APO is used only after the audio service restarts. Devices are enabled in Configurator by ticking them, and the APO can be disabled in Control Panel via 'Disable all enhancements' or 'Enable audio enhancements' checkboxes.",
    "source": "https://sourceforge.net/p/equalizerapo/wiki/Documentation/",
    "confidence": "high"
   },
   {
    "claim": "The Configurator's documented feature is 'Copy Device command to clipboard' after selecting a device. No documented command-line arguments for Configurator.exe were found; the GUI is the documented interface.",
    "source": "https://sourceforge.net/p/equalizerapo/wiki/Documentation/ ; https://sourceforge.net/p/equalizerapo/wiki/Configuration%20reference/",
    "confidence": "medium"
   },
   {
    "claim": "The APO runs inside audiodg.exe under the Audio service and cannot read files under C:\\Users\\<username>. The log is at C:\\Windows\\ServiceProfiles\\LocalService\\AppData\\Local\\Temp\\EqualizerAPO.log. Moving config outside the config directory breaks instant-mode live updates.",
    "source": "https://github.com/dechamps/APO",
    "confidence": "medium"
   },
   {
    "claim": "Windows 11 24H2 issues found: LFX/GFX and SFX/EFX modes broke Creative Sound Blaster Surround effects (SourceForge ticket 332, suggested WASAPI shared mode). ARM laptop with USB DAC had no sound with APO on 24H2 (ticket 281). Windows 11 built-in enhancements and spatial sound can reorder or replace the APO chain. No official Microsoft or Equalizer APO fix for 24H2 was found.",
    "source": "https://sourceforge.net/p/equalizerapo/tickets/332 ; https://sourceforge.net/p/equalizerapo/tickets/281 ; https://windowsreport.com/equalizer-apo-not-working-windows-11/",
    "confidence": "medium"
   },
   {
    "claim": "Version: the latest Configurator package found was EqualizerAPO-x64-1.4.2.exe (12.0 MB) on SourceForge. The version as of 2026 was not verified beyond this listing.",
    "source": "https://sourceforge.net/projects/equalizerapo/files/Configurator/",
    "confidence": "medium"
   }
  ],
  "snippets": [
   "Device: High Definition Audio Device Speakers; Benchmark",
   "HKLM\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\MMDevices\\Audio\\Render\\{GUID}\\FxProperties",
   "CLSID (GFX sAPO): {EC1CC9CE-FAED-4822-828A-82A81A6F018F}; HKLM\\SOFTWARE\\Classes\\CLSID\\{EC1CC9CE-FAED-4822-828A-82A81A6F018F}\\InprocServer32 default = C:\\Program Files\\EqualizerAPO\\EqualizerAPO.dll",
   "Config: C:\\Program Files\\EqualizerAPO\\config\\config.txt",
   "Configurator: C:\\Program Files\\EqualizerAPO\\Configurator.exe",
   "Post-mix GFX FxProperties value (listed, purpose not confirmed): {d04e05a6-594b-4fb6-a80d-01af5eed7d1d},2"
  ],
  "gotchas": [
   "If the Device pattern does not match, every following command except Device is ignored. A stale GUID (after a driver reinstall) silently disables the EQ on that device, so the app must re-check the GUID.",
   "Endpoint GUIDs change when the audio driver is reinstalled. Re-derive the GUID from MMDevices at runtime, do not hard-code it.",
   "The Device line may match on name, connection name or GUID. Using the Configurator-generated text reduces mistakes. If the match is loose (e.g. a generic name like 'Speakers'), it can apply the EQ to the wrong device.",
   "Windows enhancements ('Disable all enhancements' / 'Enable audio enhancements') can disable APOs. Detection of 'enabled' should check this too.",
   "Program Files is admin-writable by default, so a non-admin Clarim process may not be able to write config.txt there (my guess, not sourced). Clarim's LocalSystem service would be the writer, or the installer would need to set ACLs on the config folder.",
   "Equalizer APO only takes effect after a reboot or audiosrv restart. Any detection of 'working' right after enabling will be false.",
   "The wiki does not document Configurator.exe command-line arguments. Do not rely on CLI flags; drive the GUI or write config files directly."
  ],
  "open_questions": [
   "Exact registry value names for InstallPath / ConfigPath under HKLM\\SOFTWARE\\EqualizerAPO are unverified. Check the key on a real install (regedit or reg query) before depending on them. Detection via the EqualizerAPO.dll file and the CLSID InprocServer32 key is the more reliable path found.",
   "Which FxProperties value Equalizer APO writes to enable a device (CLSID value vs the {d04e05a6...},2 GFX property) is unverified. Inspect an endpoint with Configurator enabled and compare the FxProperties keys before and after.",
   "Whether the Device pattern accepts the bare GUID with braces, or needs the GUID without braces or with the {0.0.0.00000000}.{...} prefix, is not documented. Test with the Configurator-generated Device line.",
   "Current Equalizer APO version as of 2026 and any 24H2-specific fixes were not confirmed beyond the 1.4.2 Configurator package listing.",
   "Whether the Windows 11 24H2 Creative/USB DAC issues affect a generic Bluetooth headset (WZ-BT5.0) or Realtek output is untested."
  ]
 },
 {
  "facts": [
   {
    "claim": "Equalizer APO 1.4.2 is the latest Configurator download (x64 installer ~12 MB). From 1.4.2, on Windows 8.1+ the default install mode is SFX/EFX, so most users no longer need the troubleshooting options.",
    "source": "https://sourceforge.net/projects/equalizerapo/files/Configurator/ and search summary of 1.4.2 notes (secondary: https://deepwiki.com/mirror/equalizerapo shows 1.4.0, so SourceForge is likelier current)",
    "confidence": "medium"
   },
   {
    "claim": "Install mode guidance: in the Configurator 'Install as' dropdown (shown after ticking troubleshooting options) try SFX/EFX first, then SFX/MFX if SFX/EFX is unavailable. Combined Windows 11 Bluetooth stereo + hands-free endpoints use SFX/MFX by default because EFX does not work for that configuration.",
    "source": "Search results summarizing https://sourceforge.net/p/equalizerapo/ maintainer advice (Configurator/Documentation pages); https://sourceforge.net/p/equalizerapo/wiki/Documentation/",
    "confidence": "medium"
   },
   {
    "claim": "Official install steps: run setup (default path C:\\Program Files\\EqualizerAPO); Configurator.exe opens during setup to choose the audio device to install the APO to; reboot when prompted, since the APO is only used after the audio service restarts. To change devices later run C:\\Program Files\\EqualizerAPO\\Configurator.exe again.",
    "source": "https://sourceforge.net/p/equalizerapo/wiki/Documentation/",
    "confidence": "high"
   },
   {
    "claim": "APO is installed per audio endpoint. A Bluetooth headset typically appears as two endpoints (stereo and hands-free); installing on the laptop speakers does not install it on a Bluetooth headset, USB DAC, or virtual cable.",
    "source": "Search summary of Equalizer APO documentation and install guidance (https://sourceforge.net/p/equalizerapo/wiki/Documentation/)",
    "confidence": "medium"
   },
   {
    "claim": "Bluetooth on Windows 11 is a known weak spot: ticket #196 (opened 2022-01-02) reports Equalizer APO does not work with Bluetooth audio on Windows 11. It is open, unassigned, with no maintainer reply. The reporter's workaround was routing through FxSound Enhancer. Other reports cover Bluetooth devices that stop working or distort, and crackling with SFX/MFX (ticket #274, 'Bluetooth MFX crackling').",
    "source": "https://sourceforge.net/p/equalizerapo/tickets/196 ; https://sourceforge.net/p/equalizerapo/tickets/274 ; https://sourceforge.net/p/equalizerapo/tickets/193 (Bluetooth static with Graphic EQ)",
    "confidence": "high"
   },
   {
    "claim": "Graphic EQ on Bluetooth may add an electrical-interference-like noise. The maintainer's suggestion was to use peak (PK) filters instead of GraphicEQ.",
    "source": "https://sourceforge.net/p/equalizerapo/tickets/193 (as summarized by search)",
    "confidence": "medium"
   },
   {
    "claim": "Troubleshooting for unstable playback or recording: tick the troubleshooting options, then uncheck both 'Use original APO' checkboxes (this removes some sound card driver features). Unchecking 'Use original APO' also stopped Windows from forcing audio enhancements on in user reports. You can untick one 'Install APO' box to install only in the pre-mix or post-mix stage.",
    "source": "https://sourceforge.net/p/equalizerapo/wiki/Documentation/ ; https://sourceforge.net/p/equalizerapo/tickets/259 (Windows enhancements forced on)",
    "confidence": "medium"
   },
   {
    "claim": "Disabling Windows 'Audio enhancements' on a device where APO is installed disables APO too, since APO works as an enhancement. Windows Updates can reset or break the setup; the maintainer says running the Configurator usually repairs it.",
    "source": "Search summary of https://sourceforge.net/p/equalizerapo/tickets/ threads (e.g. 185, 199, 259, 261) and TechPowerUp forums",
    "confidence": "medium"
   },
   {
    "claim": "Config syntax (official reference): 'Preamp: <negative number> dB' (e.g. Preamp: -6.5 dB); 'GraphicEQ: <Frequency> <Gain dB>; <Frequency> <Gain dB>; ...'; Filter lines like 'Filter 1: ON PK Fc 50.0 Hz Gain -10.0 dB Q 2.50', 'ON LS Fc 300 Hz Gain 5.0 dB', 'ON HS Fc 1000 Hz Gain -3.0 dB'. Also 'Include: <file>' and 'Device: <pattern1>; <pattern2>' (matches device name, connection name or GUID). Unknown command names are silently ignored.",
    "source": "https://sourceforge.net/p/equalizerapo/wiki/Configuration%20reference/ (fetched) ; https://sourceforge.net/p/equalizerapo/wiki/Documentation/",
    "confidence": "high"
   },
   {
    "claim": "Positive-gain filters can clip the output; lowering the Preamp compensates.",
    "source": "https://sourceforge.net/p/equalizerapo/wiki/Documentation/",
    "confidence": "high"
   },
   {
    "claim": "Default config location reported as C:\\Program Files\\EqualizerAPO\\config\\config.txt. The official reference fetch did not state a default path, only a ConfigPath registry value under HKLM\\SOFTWARE\\EqualizerAPO.",
    "source": "Secondary search summaries (sourceforge wiki, head-fi / other guides); HKLM ConfigPath from https://sourceforge.net/p/equalizerapo/wiki/Configuration%20reference/",
    "confidence": "low"
   },
   {
    "claim": "VB-Cable with Equalizer APO (HeSuVi guide): install APO, install VB-Cable (no reboot), disable 'CABLE Output' in Recording, tick the CABLE Input device in the Configurator, use 'Restart Audio Service', set CABLE Input as default playback, and enable Exclusive Mode on the real output device. VB-Cable does not support volume control. This is the HeSuVi routing guide, not an official Equalizer APO guide.",
    "source": "https://sourceforge.net/p/hesuvi/wiki/Help/ ; VB-Cable manual https://vb-audio.com/Cable/VBCABLE_ReferenceManual.pdf",
    "confidence": "medium"
   },
   {
    "claim": "License: multiple SourceForge/aggregator pages list the core Equalizer APO project as GPL v2 (GPLv2). Related projects (AQUA, eqapo64 fork, EQUALIZER-MASTER) are GPLv3. The official wiki fetch did not show a license statement.",
    "source": "Search summaries of https://sourceforge.net/projects/equalizerapo/ and https://github.com/h39s/AQUA ; aggregator text (chocolatey / sourceforge listings)",
    "confidence": "medium"
   },
   {
    "claim": "Uninstall: no official clean-removal procedure found. Community advice: uninstall the Configurator first, then Equalizer APO, restart; if sound does not return, reinstall or repair the audio driver. Silent uninstall prompts 'Remove configurations and registry backups'. A leftover EqualizerAPO.log can remain in C:\\Windows\\ServiceProfiles\\LocalService\\AppData\\Local\\Temp.",
    "source": "Search summaries of https://sourceforge.net/p/equalizerapo/tickets/239 (Changes remain after uninstalling), /188, /351, /199",
    "confidence": "medium"
   },
   {
    "claim": "CPU cost of Equalizer APO was not found in any source checked.",
    "source": "No source found",
    "confidence": "low"
   }
  ],
  "snippets": [
   "Preamp: -6.5 dB",
   "GraphicEQ: 20 -3; 100 2; 1000 0; 8000 -2; 16000 1",
   "Filter 1: ON PK Fc 50.0 Hz Gain -10.0 dB Q 2.50",
   "Filter 2: ON LS Fc 300 Hz Gain 5.0 dB",
   "Filter 3: ON HS Fc 1000 Hz Gain -3.0 dB",
   "Device: Fones de ouvido (WZ-BT5.0)",
   "Include: <file name>",
   "C:\\Program Files\\EqualizerAPO\\Configurator.exe (re-run to change devices)"
  ],
  "gotchas": [
   "Bluetooth headsets expose two endpoints (A2DP stereo and HFP hands-free). APO must be installed on the stereo endpoint that the user actually plays to; the hands-free endpoint needs SFX/MFX and may crackle. Install mode choice matters per endpoint.",
   "Ticket #196 (Bluetooth not working on Windows 11) is open with no maintainer reply, so Bluetooth support is not guaranteed. Test on the WZ-BT5.0 before promising it.",
   "GraphicEQ on Bluetooth reportedly adds noise; the maintainer suggested PK filters instead. Prefer Filter PK/LS/HS lines for the Graves/Medios/Agudos sliders and presets.",
   "Positive gain can clip; keep the Preamp negative (e.g. Preamp: -X dB matching the largest boost).",
   "Windows Audio Enhancements and Windows updates can disable or reset APO. Clarim should detect and tell the user to re-run Configurator.exe or 'Restart Audio Service'.",
   "Install as APO on the Bluetooth endpoint may only be picked in the Configurator GUI; config writing alone may not install the APO on the correct device.",
   "VB-Cable is not needed for direct per-device EQ on the headset; installing APO on CABLE devices is only for the HeSuVi-style routing, and VB-Cable has no volume control.",
   "Uninstall is not cleanly documented; leftover registry and log entries are reported. Advise a reboot and an audio-driver repair if sound does not return."
  ],
  "open_questions": [
   "Does Clarim running as a normal user have write access to C:\\Program Files\\EqualizerAPO\\config? Not verified. Likely needs admin or a per-user ConfigPath via HKLM registry, which is an open question.",
   "Do config file changes apply live, or only after a 'Restart Audio Service'? Not confirmed from official docs.",
   "What is the CPU cost of Equalizer APO with a few PK filters on a low-end PC? Not found.",
   "Does Equalizer APO 1.4.2 on Windows 11 24H2 with the WZ-BT5.0 work in SFX/EFX or SFX/MFX mode? Not tested; only forum anecdotes from 2021-2022.",
   "Exact license: GPLv2 is the likely answer from secondary pages; the license text should be checked in the SourceForge repository or download package before redistributing or bundling any APO binaries."
  ]
 }
]
```
