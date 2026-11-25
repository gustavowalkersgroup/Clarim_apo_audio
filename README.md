# Clarim

Ferramenta leve e de código aberto para Windows 10/11 que faz duas coisas:

1. **Microfone limpo**: pega o seu microfone real, tira ruído com IA (RNNoise ou DeepFilterNet3), segura vozes de fundo, mantém o volume da sua voz estável e entrega tudo num **microfone virtual**. Discord, Meet, Zoom e companhia usam esse microfone virtual.
2. **Canais de saída** Jogo / Chat / Mídia / Aux: cada programa que toca áudio pertence a um canal, e cada canal tem sua saída (fone, caixa...), seu volume e seu mudo. Spotify e YouTube na Mídia, Discord e Meet no Chat, jogos no Jogo.

Roda sem janela, como um agente em segundo plano (ícone na bandeja), e é controlado por um painel local em `http://127.0.0.1:47800`. Sem Electron, sem nuvem: um único `clarim.exe`.

## Instalar

Você só precisa do `clarim.exe`. Nada de compilar, nada de instalar outros programas antes.

1. Baixe o **`clarim.exe`** na página de versões (*Releases*) do projeto no GitHub.
2. Dê **duplo clique** nele. O Clarim pergunta "Instalar o Clarim neste computador?": clique em **Sim**.
3. O Windows pede permissão de administrador, **uma vez só**: aceite. Como o `clarim.exe` não tem assinatura digital paga, o Windows pode mostrar "Editor desconhecido" (e o SmartScreen, "O Windows protegeu o computador": clique em "Mais informações" e depois em "Executar assim mesmo").
4. O Clarim baixa o VB-Cable e avisa que vai abrir o instalador dele (da VB-Audio). Na janela que abrir, clique em **Install Driver** e depois em OK.
5. Espere um pouco: o Clarim baixa e instala sozinho o Equalizer APO (cerca de 1 minuto; o som pode piscar). No fim aparece um **resumo** do que foi feito e do que ficou pendente, se ficou.
6. O painel do Clarim abre (se não abrir, use o ícone do Clarim na bandeja ou o menu Iniciar) e, na primeira instalação, mostra **Primeiros passos**. Falta só uma coisa: no **Discord, Meet ou Zoom**, escolha **CABLE Output** como microfone (e desligue o controle automático de ganho do app). Clique em **Entendi** quando terminar; o cartão some.

Pronto. O Clarim passa a iniciar sozinho com o Windows, com um ícone na bandeja. Se o resumo disser para **reiniciar o computador** (às vezes o VB-Cable só termina de instalar depois disso), reinicie.

- **O que a instalação faz:** copia o `clarim.exe` para `C:\Program Files\Clarim`, registra o serviço do Clarim (início automático), e instala o **VB-Cable** e o **Equalizer APO** só se ainda não estiverem no PC (quem já tem um deles não tem o programa mexido). Cada download é conferido contra um SHA-256 fixado no Clarim; se não bater, nada é executado. O registro fica em `C:\Program Files\Clarim\install.log`.
- **Sobre o VB-Cable:** é um programa da VB-Audio, **grátis por doação** (*donationware*): se gostar, apoie em <https://vb-cable.com>. O Clarim **não inclui nem redistribui** o VB-Cable: a licença da VB-Audio não permite embuti-lo em outro instalador. Por isso o Clarim baixa o pacote **oficial, sem modificação**, do site da VB-Audio, confere o SHA-256, e abre o instalador **da própria VB-Audio** numa janela normal, para você clicar. Se o pacote no site da VB-Audio mudou (hash diferente), o Clarim não executa nada e abre <https://vb-audio.com/Cable/> para você instalar à mão.
- **Clicou em "Não"?** O Clarim roda sem instalar nada, só enquanto estiver aberto (use "Sair" na bandeja para fechar), como sempre foi. Para instalar depois: `clarim.exe setup`, ou abra o `clarim.exe` de novo. Rodar `setup` outra vez só instala o que falta.
- **Instalação manual, sem pergunta nenhuma:** veja "Instalar só o serviço (avançado)".

## Requisitos

- Windows 11 (o roteamento por programa foi verificado nele). No Windows 10 o Clarim trabalha só com o volume dos canais; veja "Como funcionam os canais".
- **VB-Cable** (gratuito, da VB-Audio) para o microfone virtual: a instalação acima cuida dele. Sem ele os canais funcionam, mas o painel avisa que não há microfone virtual e o cartão "Primeiros passos" tem o botão "Instalar VB-Cable" (abre <https://vb-audio.com/Cable/>).
- **Equalizer APO** (gratuito, opcional) só para o equalizador de saída: a instalação acima cuida dele; se você pular, o **Clarim instala e configura sozinho**, pelo painel, com o serviço do Clarim instalado. Veja "Equalizador".
- Para compilar: Rust estável (toolchain MSVC) e as Build Tools do Visual Studio.
- WebView2 Runtime (já vem no Windows 11 e com o Edge) para a janela própria do painel. Se não houver, o painel abre no Edge (no seu perfil normal) ou no navegador padrão.

## Compilar

```
cargo build --release
```

O executável fica em `target\release\clarim.exe`. O DeepFilterNet3 ("Forte") entra como dependência git na primeira compilação (precisa de rede e de `git`); sem a feature `dfn` do `clarim-audio` sobra só o RNNoise.

## Primeiro uso

Depois da instalação (veja "Instalar"), o Clarim já roda sozinho: o painel abre no fim, e depois use o menu Iniciar (Clarim) ou o ícone na bandeja. Sem instalar nada (por exemplo, para testar uma compilação sua), execute `clarim.exe agent` (ou `clarim.exe` e responda **Não** à pergunta de instalação): ele sobe o agente neste processo, põe o ícone na bandeja e abre o painel. Com o agente já rodando, `clarim.exe` só abre o painel.

O painel mostra o cartão **Primeiros passos** até você clicar em **Entendi**: ele lembra de escolher **CABLE Output** como microfone no Discord, Meet ou Zoom e diz se o VB-Cable foi encontrado (se não, tem o botão **Instalar VB-Cable**). Quem já usava o Clarim antes dele existir não vê o cartão.

- Clique esquerdo no ícone abre o painel; clique direito mostra "Abrir painel", "Pausar microfone" e "Sair".
- Fechar a janela do painel **não** para o Clarim. Para parar, use "Sair" na bandeja.
- O painel abre numa **janela própria do Clarim** (`clarim.exe window`), com o efeito **Acrylic** do Windows 11: o papel de parede aparece desfocado atrás dos cartões. Ela lembra o tamanho e a posição. Se os "Efeitos de transparência" estiverem desligados (Configurações > Personalização > Cores), ou o tema for de alto contraste, a janela fica sólida, como antes. Só uma janela por vez: abrir de novo traz a que já existe para a frente.
- Na primeira execução, se o dispositivo padrão do Windows for virtual (por exemplo, se o "CABLE Input" do VB-Cable foi definido como saída padrão), os quatro canais já são apontados para a primeira saída real encontrada. Ajuste no painel. Se o Clarim não conseguir listar os dispositivos de áudio nessa primeira vez, ele não grava nenhuma configuração inicial e tenta de novo na próxima vez que iniciar.

Para testar sem mexer em nada: `clarim.exe agent --dry-run` (só observa os programas, não liga o microfone) ou `clarim.exe agent --no-routing` (liga o microfone, mas não mexe em roteamento nem volume dos programas).

## Consumo

Medido num PC de mesa (build release, Windows 11, roteamento só observando, painel fechado): o agente usa em torno de **2% de um núcleo** com a supressão em Desligado ou Leve e em torno de **17%** com Forte (o DeepFilterNet3 gasta cerca de 1,5 ms a cada quadro de 10 ms; é o preço da supressão mais forte). Com o painel aberto sobe uns 0,1 a 0,2 ponto percentual. Com o "Roteamento por app" ligado a varredura dos programas passa de a cada 2 s para a cada 1 s, o que soma, por estimativa, mais uns 0,5 ponto percentual (não medido: medir exigiria mexer na saída dos seus programas).

A latência estimada do caminho do microfone, só dentro do Clarim, é de cerca de **18 ms** (Desligado), **28 ms** (Leve) e **49 ms** (Forte). Ela não inclui o mixer do Windows, o driver do cabo nem o buffer do programa que lê o cabo. `clarim.exe selftest` mede o processamento no seu PC.

## Instalar só o serviço (avançado)

A instalação guiada (`clarim.exe setup`, ou o duplo clique) usa isto por baixo e ainda cuida do VB-Cable e do Equalizer APO. Se você quer **só o serviço**, sem perguntas, sem VB-Cable e sem Equalizer APO, num prompt **como administrador**:

```
clarim.exe install
```

Isso copia o programa para `C:\Program Files\Clarim\clarim.exe`, registra o serviço **Clarim** (início automático), o inicia e cria a entrada **Clarim** no menu Iniciar. O Clarim passa a subir sozinho quando você entra no Windows; abra o painel pelo menu Iniciar (Clarim) ou pelo ícone na bandeja. O serviço não processa áudio: ele mantém o agente rodando na sua sessão e o reinicia se ele cair (se você escolher "Sair" na bandeja, ele não volta até o próximo logon), e faz, a pedido do painel, o que o equalizador de saída precisa de administrador: instalar o Equalizer APO, ligá-lo nos aparelhos e reiniciar o Windows Audio (veja "Equalizador").

**Instale só a partir de uma pasta em que apenas administradores gravam.** O `install` copia o `clarim.exe` que você está executando e o serviço roda como SYSTEM; se a pasta de origem aceitar escrita de qualquer usuário, outro programa poderia trocar arquivos ali antes. Se tiver dúvida, copie você mesmo o `clarim.exe` para `C:\Program Files\Clarim` e rode `install` de lá.

Para remover, **primeiro feche o Clarim** ("Sair" na bandeja) e depois, como administrador:

```
clarim.exe uninstall
```

Isso para o serviço, remove o registro e a entrada do menu Iniciar, tira do Equalizer APO (se estiver instalado) o arquivo e a linha do Clarim (veja "Equalizador") e apaga a pasta em Program Files, **menos** `C:\Program Files\Clarim\backup` (a cópia das configurações originais dos aparelhos em que o Equalizer APO foi ligado). A pasta apagada inclui a cópia do pacote do VB-Cable que a instalação guiada deixa em `C:\Program Files\Clarim\cache`. O `uninstall` **não** remove o VB-Cable (desinstale em Configurações > Aplicativos) nem o Equalizer APO, e não desliga o equalizador dos aparelhos: para isso veja "Equalizador > Como desfazer". Suas configurações em `%APPDATA%\Clarim` ficam. As rotas por programa que o Clarim gravou no Windows também ficam: veja "Desfazer o roteamento".

## Usar o microfone virtual no Discord, Meet, Zoom

No programa, escolha como **microfone/entrada**: **CABLE Output (VB-Audio Virtual Cable)**. É o lado de captura do cabo; o Clarim escreve no lado "CABLE Input". No painel, em "Microfone", a entrada deve ser o seu microfone **real**. Se você escolher o próprio CABLE Output como entrada, o Clarim recusa, porque causaria eco em loop.

Dica: desative o **controle automático de ganho** (no Discord, "Controle automático de ganho") e a **supressão de ruído** do próprio Discord/Meet/Zoom, para não processar duas vezes. O ajuste automático de volume do programa briga com o "Volume automático" do Clarim: os dois ficam correndo atrás um do outro e o volume da sua voz sobe e desce. Veja "Volume automático".

## Volume automático

A chave **Volume automático** (cartão "Microfone", coluna "Tom e nível") vem ligada e mantém a sua voz no mesmo volume na chamada: não estoura nem fica baixa. Embaixo dela o painel mostra o ganho de agora e o nível da sua voz, tipo "ganho +6 dB · voz −23 dBFS".

- **Alvo**: o volume da sua fala fica perto de **−23 dBFS** (a média, em dB, do volume de cada pedacinho de 10 ms em que você está falando).
- **Aprende só com a sua voz**: só conta o que o Clarim reconhece como fala sua. Silêncio, ruído e voz bem mais baixa que a sua (alguém longe, som vazando) não mexem no nível. Ele só começa a ajustar depois de ouvir você falar um pouco.
- **Ganho de −12 a +20 dB**: sobe devagar (cerca de 3 dB por segundo) e desce rápido (cerca de 10 dB por segundo), então uma voz mais alta é segurada logo. O limitador de −1 dBFS continua no fim da cadeia, e o ganho ainda recua sozinho se o pico da voz chegar perto do limite.
- **Não levanta o ruído**: o ganho nunca sobe a ponto de deixar o ruído entre as palavras acima de −55 dBFS. Num microfone com muito ruído de fundo o ganho máximo fica menor, e a voz pode chegar abaixo do alvo.
- **Desligar**: tire a chave. O ganho volta a 0 dB em menos de 1 s, sem estalo, e a voz sai como o microfone entrega (mais os ganhos de entrada e de saída que você ajustou).
- **Só um controle automático por vez**: desligue o do programa da chamada (veja a dica acima).

### Se o microfone estourar: volume do Windows

Quando o sinal do próprio microfone estoura (corta no topo) enquanto você fala, nada que vem depois conserta. Com o Volume automático ligado e a chave **Ajustar o volume do microfone no Windows se estourar** ligada (em "Avançado", vem ligada), o Clarim mexe no volume do microfone real no Windows, **só durante chamadas**:

- **Abaixa em passos de 3 dB**, no máximo **12 dB no total**, com pelo menos 10 s entre um passo e outro. Bater na mesa (sem voz) não conta como estouro.
- **Só em microfones com volume de hardware**: é o controle de "Níveis" do microfone no Windows. Se o seu não tem (ou o volume já está no mínimo), o Clarim não mexe em nada e o painel diz: abaixe o "Aumento do microfone" (Propriedades do microfone > Níveis) ou afaste o microfone.
- Se abaixar não adianta (os estouros não caem pelo menos pela metade), ele devolve o que tirou nessa rodada, **para de tentar até o Clarim reiniciar** e o painel mostra o mesmo aviso de microfone sem volume de hardware.
- **Devolve** em passos de 3 dB, até o que ele mesmo tirou, quando a sua voz continua mais de 6 dB abaixo do alvo mesmo com o Volume automático no máximo (+20 dB) por uns 10 s, sem estourar há 1 minuto. Nunca passa do volume que o microfone tinha antes. Ao **sair do Clarim** ("Sair" na bandeja) ele também devolve o que tirou, se você não mexeu no volume nesse meio-tempo; se mexeu, só esquece a anotação.
- O quanto ele tirou fica anotado no `config.json` (`mic_volume_taken`, não edite). Se o Clarim fechar sem avisar, a anotação fica e ele devolve na próxima chamada em que a voz estiver baixa (com o mesmo microfone). Se você subir o volume no Windows por conta própria, ele percebe na hora de devolver (ou ao sair) e só apaga a anotação.
- Depois de cada ajuste o painel mostra uma nota por 1 minuto, e ao abaixar a bandeja dá um balão discreto, sem som ("Microfone estourando"), uma vez por chamada. A calibração do Foco na voz acompanha o novo volume, então você não precisa recalibrar.
- **Desligar**: tire a chave. O Clarim para de mexer no volume; o que ele já tirou volta ao sair, ou você mesmo sobe o volume no Windows.

## Avisos de vazamento na chamada

Mesmo com a supressão ligada, às vezes ainda passa som para o microfone virtual. **Só durante chamadas**, o Clarim avisa quando percebe:

- **Som das caixas** (eco): o que toca nas caixas, de qualquer programa, entra no seu microfone e chega nos outros. O Clarim compara o volume do que toca nas caixas com o do que sai pro microfone virtual, e vê se um repete o outro com atraso. Ele olha só o volume (a cada 10 ms); não grava nem guarda áudio.
- **Ruído de fundo**: ruído que não é eco e ainda passa pelo filtro entre as suas falas.

**Quando é "em chamada"**: quando algum programa (menos o OBS) está usando o **CABLE Output** como microfone. O painel mostra "Em chamada: Discord" no cartão do microfone. O Clarim leva até uns 3 s para notar que a chamada começou e segura o estado por uns 8 s depois que ela termina. Fora de chamada ele não olha as caixas.

**Como avisa** (discreto):

- Um **balão no ícone da bandeja, sem som**: "Som vazando na chamada" ou "Ruído passando na chamada". Um por tipo em cada chamada; o mesmo aviso só volta depois de 10 min seguidos de vazamento.
- **Em tela cheia, apresentação ou com o Windows "ocupado"** (um jogo, por exemplo) o balão não aparece: ele espera e aparece quando você der alt-tab.
- Um **aviso amarelo no painel**, que fica enquanto o vazamento durar, e a dica do ícone da bandeja vira "Clarim · som vazando na chamada".
- Não avisa na hora: o vazamento precisa durar alguns segundos (no eco, até uns 20 s) para não dar alarme falso, e o aviso some uns 10 s depois que ele para.
- Em "Avançado", durante uma chamada, uma linha mostra os números ("Vazamento: eco 0,12 · atraso 250 ms ..."), e o `clarim.log` ganha uma linha a cada 10 s com as mesmas medidas.

O que fazer: para o eco, abaixe o volume das caixas ou use fone; para o ruído, aproxime o microfone, ligue o Foco na voz e use a supressão Forte.

**Limites conhecidos:**

- **O programa da chamada pode já cortar o próprio eco.** Discord (cancelamento de eco ligado por padrão) e Meet/Chrome cortam o eco do áudio deles mesmos depois do Clarim, no CABLE Output. O Clarim vê o som das caixas inteiro e não separa esse áudio do resto, então o aviso de eco vale mais para o som de **outros** programas: voz de jogo, YouTube, podcast. Só com música, o DeepFilterNet3 (Forte) já tira quase tudo, e não avisar é o resultado certo.
- **Eco muito alto não é medido.** Eco a menos de 10 dB do volume da sua própria voz se confunde com você falando. Um vazamento desse tamanho é óbvio de qualquer jeito.
- **O balão é pulado enquanto você está ocupado** (tela cheia etc.). O aviso no painel está sempre lá.

**Desligar**: tire a chave **Avisar se vazar som na chamada** (coluna "Voz"). Desligada, o Clarim nem lê o volume das caixas.

## Como funcionam os canais

- Cada **programa** (identificado pelo nome do `.exe`) pertence a um canal. Programas conhecidos já vêm com regra (Discord, Teams, Zoom no Chat; Spotify, navegadores, VLC na Mídia; OBS no Aux). Quem não tem regra cai no canal padrão, **Jogo**.
- Os **navegadores** (Chrome, Edge, Firefox, Brave...) vêm na **Mídia**, para o YouTube e a música. Quando um navegador começa a usar um microfone (uma chamada no Meet, no Teams web, no Discord web), ele passa para o **Chat** sozinho e volta para a Mídia cerca de 20 s depois do fim da chamada. Na lista do painel ele aparece no Chat com a marca "em chamada", e a lista dele continua mostrando a regra (Mídia). Para desligar isso, ponha `"call_to_chat": false` no `config.json`.
- No painel, cada programa que está tocando aparece no seu canal; escolha outro canal na lista dele para mover. Isso grava uma regra. "Sem regra" apaga a regra e o programa volta ao canal padrão.
- O roteamento usa a escolha de saída **por programa do próprio Windows** (o mesmo recurso que o EarTrumpet usa): nenhum driver, nenhum atraso extra. O volume e o mudo do canal são aplicados ao volume de cada programa do canal.
- Isso foi verificado no **Windows 11**. No **Windows 10** o Clarim trabalha só com o volume dos canais, sem trocar a saída de cada programa.
- Um programa que está tocando numa saída diferente da do canal aparece com a marca "não roteado" (Windows sem esse recurso, ou programa que ignora a troca). A marca só vale para canais com uma saída escolhida: num canal em "Padrão do Windows" cada programa segue o padrão do papel que ele usa (chamadas podem ter outro padrão que o do som geral), então não há o que marcar.
- É **por processo**: o navegador é um programa só. Todas as abas do Chrome vão para o mesmo canal; não dá para separar uma aba da outra.
- O volume do canal é aplicado quando o som de um programa aparece e quando você mexe no controle do canal, então um ajuste feito no mixer do Windows vale até um desses dois momentos. O volume **não** volta ao normal quando você fecha o Clarim: os programas ficam no volume em que estavam. Desligar o "Roteamento por app" devolve ao padrão o volume e o mudo que o Clarim mexeu nesta execução. Um programa que o Clarim nunca mexeu, num canal em 100% e sem mudo, não é tocado: ele mantém o volume que já tinha. O Windows lembra o volume por programa, então um programa que o Clarim já mexeu nesta execução é conferido toda vez que ele volta a tocar (aba nova, programa reaberto, chamada) e corrigido para o volume do canal. O que uma execução anterior do Clarim deixou gravado não é corrigido por esta regra.
- Alguns programas só passam a usar a nova saída quando o áudio é pausado e retomado, ou quando o programa é reaberto.
- A chave "Roteamento por app" desligada deixa o Clarim só **observando**. Ao desligar, o Clarim desfaz o que ele mesmo gravou nesta execução, em todos os programas abertos dos executáveis que ele mexeu (o Windows guarda a saída e o volume por executável, então um programa reaberto herda o que o Clarim gravou): a saída deles volta a seguir o dispositivo padrão do Windows e o volume e o mudo voltam ao normal. Programas que já fecharam, ou rotas gravadas em execuções anteriores ou por outras ferramentas, ficam no Windows (veja "Desfazer o roteamento"). Fechar o Clarim não desfaz nada.

## Equalizador

**Microfone.** No cartão "Microfone", na coluna "Tom e nível", escolha o equalizador (Neutro, Clareza, Quente ou Locutor) e, se quiser, ajuste **Graves**, **Médios** e **Agudos** (de −12 a +12 dB). Os três controles somam ao equalizador escolhido e valem só para a sua voz, antes do microfone virtual.

**Saída (o que você ouve).** No cartão "Equalizador de saída" escolha um aparelho (fones, caixas) e uma predefinição (Neutro, Mais graves, Clareza ou Agudos) ou ajuste Graves, Médios e Agudos à mão (de −12 a +12 dB). Vale **por aparelho**, não por canal: afeta tudo que toca naquele aparelho. Os canais do Clarim não processam o som dos programas (só escolhem a saída e o volume), por isso o equalizador de saída é feito pelo **Equalizer APO** (gratuito, de código aberto), que o **Clarim instala e configura por você**: você nunca abre o instalador, o DeviceSelector nem o Editor do Equalizer APO. Tudo pelo painel, e precisa do serviço do Clarim (`clarim.exe install`, como administrador), porque instalar e configurar o áudio do Windows exige privilégio de administrador:

1. No cartão, clique em **Instalar equalizador**. O Clarim baixa o Equalizer APO (cerca de 12 MB), instala (leva uns 1 min, o som pode piscar) e liga o equalizador nos aparelhos que os seus canais usam (nunca no cabo virtual do microfone nem em dispositivos virtuais).
2. Quando o painel avisar **"Falta reiniciar o áudio pra valer"**, clique em **Reiniciar áudio agora**. O som some por uns segundos e uma chamada em andamento pode cair (o microfone do Clarim volta sozinho). Só depois disso a mudança vale (ligar ou desligar um aparelho: até lá o som continua como estava).
3. Em cada aparelho, a chave **Equalizador neste aparelho** liga e desliga o equalizador dele (a mudança também pede para reiniciar o áudio). Depois o painel mostra "Ativo neste aparelho".

Se você já tinha o Equalizer APO instalado, o Clarim o reconhece e usa; os aparelhos que você já tinha configurado aparecem com a chave ligada.

- **O que o serviço faz como administrador (SYSTEM):** (1) baixa o instalador oficial **fixo** (Equalizer APO 1.4.2 x64, do SourceForge) para `C:\Program Files\Clarim\cache`, confere o SHA-256 contra o valor gravado no Clarim e, se não bater, apaga e **não executa**; (2) roda o instalador em modo silencioso e depois o apaga; (3) edita o registro de áudio do Windows (`HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render\{aparelho}\FxProperties` e `HKLM\SOFTWARE\EqualizerAPO\Child APOs`) do mesmo jeito que o DeviceSelector do Equalizer APO faz, só nos aparelhos que você liga, e liga `DisableProtectedAudioDG` (também como o DeviceSelector: a DLL do Equalizer APO não é assinada pela Microsoft, e esse valor deixa o Windows carregá-la no processo de áudio; isso afrouxa uma proteção do Windows contra programas não assinados nesse processo); (4) antes da primeira alteração de cada aparelho guarda uma cópia do estado original em `C:\Program Files\Clarim\backup\<id do aparelho>.json`; (5) reinicia o serviço Windows Audio (e os serviços que dependem dele) quando você clica em "Reiniciar áudio agora". O serviço só aceita ids de aparelho que existem no Windows, nunca caminhos, endereços nem comandos.
- **Como desfazer:** desligue a chave **Equalizador neste aparelho** (o Clarim devolve o registro do aparelho exatamente ao que era e pede para reiniciar o áudio). Para tirar o Equalizer APO do PC, desinstale-o em **Configurações > Aplicativos > Equalizer APO** (desligue antes a chave dos aparelhos). O `clarim.exe uninstall` **não** remove o Equalizer APO. Se um aparelho ficar com o Equalizer APO ligado sem o registro dos valores originais (feito por outra ferramenta), o Clarim não mexe nele e o painel manda usar o desinstalador do Equalizer APO.
- **O que o Clarim mexe na pasta do Equalizer APO:** só o arquivo `config\clarim-eq.txt` e **uma** linha, `Include: clarim-eq.txt`, no fim do `config.txt`. Antes de mexer no `config.txt`, ele guarda uma cópia do original como `config.txt.clarim-backup`; nenhuma outra linha é tocada. Única exceção: se o `config.txt` ainda é o padrão que o instalador do Equalizer APO cria (ele baixa o volume em 6 dB e liga um reforço de graves de exemplo, o que deixaria o preset "Neutro" fora do neutro), o Clarim o substitui só pela linha `Include`, e a cópia `.clarim-backup` guarda o arquivo original. O Equalizer APO relê esses arquivos sozinho. O `clarim.exe uninstall` apaga o `clarim-eq.txt` e a linha do `config.txt` (a cópia `.clarim-backup` fica).
- **Serviço do Clarim:** sem o serviço instalado o painel só mostra o botão "Instalar equalizador" desligado, com o motivo. Se o Equalizer APO já está instalado e configurado, o equalizador de saída ainda funciona sem o serviço quando a pasta `config` do Equalizer APO aceita escrita da sua conta, mas ligar e desligar aparelhos e reiniciar o áudio sempre precisam do serviço. Se o serviço já estava instalado de uma versão anterior do Clarim, rode `clarim.exe install` de novo (como administrador) para atualizá-lo: só a versão nova instala e configura o equalizador (o painel avisa quando o serviço não responde).
- **Com o Clarim fechado:** o equalizador de saída continua valendo, porque quem o aplica é o Equalizer APO, a partir do arquivo que o Clarim deixou. Para desligar, escolha **Neutro** no painel (ou desligue a chave do aparelho).
- **Bluetooth:** o Equalizer APO pode não funcionar em fones Bluetooth (há relatos de falha e de chiado no Windows 11). O painel avisa; teste no seu aparelho.
- **Melhorias de áudio do Windows:** se estiverem desligadas no aparelho, o equalizador não age, e o painel avisa. O Windows também desliga essas melhorias sozinho quando um efeito falha ao carregar várias vezes. Ganhos positivos baixam o volume geral na mesma medida (um "Preamp" negativo igual ao maior ganho), para o som não estourar.
- **Reiniciar o áudio:** o Clarim reinicia o serviço Windows Audio e os que dependem dele. O microfone e os canais do Clarim se reconectam sozinhos (o painel mostra "Reconectando o áudio…" por alguns segundos). Programas que seguram o áudio podem precisar ser reabertos. Se o reinício falhar, o painel manda reiniciar o PC.
- **Ainda não testado ao vivo:** o formato do arquivo segue a documentação do Equalizer APO e o registro que o Clarim grava segue o código-fonte do DeviceSelector 1.4.2 (testado só com dados simulados e com os registros lidos de um PC com o Equalizer APO). O download, a instalação, a ligação dos aparelhos e o reinício do áudio feitos pelo próprio Clarim **não foram executados de verdade**: a primeira vez deve ser acompanhada. O SHA-256 fixado do instalador veio do pacote da comunidade Chocolatey (o SourceForge não publica um) e foi conferido contra um download real em 25/11/2026: bate. Se um dia deixar de bater, a instalação recusa com "hash diferente". Veja `docs/eqapo.md`.
- `--dry-run` deixa o equalizador de saída desligado (só observa).

## Foco na voz: o que ele faz e o que não faz

O Foco na voz **não** separa a sua voz de outras vozes. Não existe, hoje, um modelo aberto leve o bastante para isso. O que ele faz: depois da supressão de ruído, abaixa fortemente o som (cerca de 30 dB) sempre que o nível ficar abaixo do da sua voz calibrada menos a sensibilidade, ou quando não há fala detectada.

Funciona porque a sua voz, perto do microfone, costuma chegar 15 a 25 dB mais alta que a de gente do outro lado da sala. Por isso:

- Calibre ("Calibrar minha voz", 5 s falando normalmente, na posição em que você usa o microfone) e recalibre se mudar o microfone, a distância ou o ganho de entrada.
- Quem fala perto do microfone, uma TV alta ou alguém no mesmo volume que você passam.
- Enquanto alguém de fundo fala ao mesmo tempo que você, essa voz também passa.

## Legado: SteelSeries Sonar / GG

O Clarim substitui o Sonar: o caminho normal é o **VB-Cable** (veja "Requisitos"), sem o Sonar instalado, e nada aqui precisa ser feito. Esta seção só vale se você ainda tiver o SteelSeries GG/Sonar no PC.

Feche o GG/Sonar enquanto usa o Clarim. O Sonar também roteia cada programa para os próprios canais virtuais, e os dois brigam pela saída de cada app. O Clarim ignora o processo do próprio Sonar e do NVIDIA Broadcast, mas não tem como arbitrar as escolhas do Sonar para os outros programas.

Por isso, se houver uma saída do Sonar instalada, na **primeira execução** o Clarim começa com o **Roteamento por app desligado**, para não mexer na saída de todos os programas de cara. O painel mostra um aviso. Feche o GG/Sonar e ligue o "Roteamento por app" quando quiser usar os canais. Se o roteamento estiver ligado e o Sonar presente, o painel avisa que os dois podem brigar. Como último recurso, se não houver nenhum outro cabo virtual, o Clarim usa o "SteelSeries Sonar - Microphone" como microfone virtual.

## Desfazer o roteamento

```
clarim.exe reset-routing --yes
```

Apaga **todas** as escolhas de saída por programa do Windows (inclusive as feitas em Configurações do Windows ou por outras ferramentas). Depois disso, cada programa volta a seguir o dispositivo padrão do Windows. Sem o `--yes` o comando só mostra o aviso e não apaga nada (não há pergunta no terminal).

Se o Clarim estiver rodando com o "Roteamento por app" ligado, ele grava as rotas dos canais de novo em alguns segundos, e o comando imprime um aviso nesse sentido. Para o reset valer, desligue o "Roteamento por app" no painel (o que já desfaz o que o Clarim gravou nos programas abertos) ou feche o Clarim em "Sair" na bandeja, e só então rode o comando.

## Segurança

O painel só escuta em `127.0.0.1` e exige um token que muda a cada execução, mas **qualquer conta de usuário deste PC** consegue abri-lo e mexer no microfone e nos canais. O Clarim parte da suposição de um PC doméstico, sem outras contas em que você não confie. Com o serviço instalado, qualquer conta interativa também pode pedir ao serviço (pelo pipe `\\.\pipe\ClarimEqApo`) que ajuste o equalizador de saída; o serviço só aceita ids de aparelho e ganhos em dB, e nunca grava texto vindo do pedido: na pasta do Equalizer APO ele só escreve o `clarim-eq.txt` (gerado por ele), acrescenta a linha `Include: clarim-eq.txt` ao `config.txt` e guarda a cópia `config.txt.clarim-backup`, sem seguir links e sem ler um `config.txt` que não seja um arquivo comum de até 1 MiB. Atenção: o instalador do Equalizer APO deixa essa pasta `config` gravável por **qualquer conta** do PC (é o desenho dele; o Clarim não muda isso), então qualquer conta pode acrescentar ali linhas como `VSTPlugin:`, que o motor de áudio carrega. O mesmo pipe aceita os pedidos "instalar o Equalizer APO", "ligar/desligar o equalizador de um aparelho" e "reiniciar o áudio": qualquer conta interativa pode pedi-los, mas o serviço só instala o instalador fixo com o SHA-256 conferido, só mexe em aparelhos de áudio que existem (nunca em caminhos ou comandos vindos do pedido) e só reinicia o Windows Audio no máximo uma vez a cada 10 s. Sem o serviço instalado nada disso existe. A instalação guiada (`clarim.exe setup`) roda como administrador, na sua sessão: ela baixa o pacote do VB-Cable de `download.vb-audio.com`, confere o SHA-256 fixado, abre o zip com o `tar.exe` do Windows (caminho absoluto em System32), recusa qualquer entrada que saia da pasta de destino e extrai, sem modificar, em `C:\Program Files\Clarim\cache\VBCABLE_Driver_Pack46` (pasta que só administradores gravam) antes de abrir o instalador da VB-Audio. O Equalizer APO é instalado pelo serviço, como descrito em "Equalizador". Como o `setup` copia o `clarim.exe` que você executou para Program Files e roda como administrador, vale o mesmo aviso do `install`: baixe o arquivo só de quem você confia e, se tiver dúvida, rode-o de uma pasta em que apenas administradores gravam.

## Linha de comando

| Comando | O que faz |
|---|---|
| `clarim.exe` | abre o painel se o agente já roda; sem o Clarim instalado, oferece a instalação guiada (`setup`); senão roda o agente neste processo |
| `clarim.exe setup [--no-open]` | instalação guiada: serviço + VB-Cable + Equalizer APO, só o que falta; pede administrador uma vez (sozinho) e termina com um resumo; `--no-open` não abre o painel no fim |
| `clarim.exe agent [--dry-run\|--no-routing]` | roda o agente (é o que o serviço inicia) |
| `clarim.exe open` | abre o painel (o agente precisa estar rodando) |
| `clarim.exe install` / `uninstall` | instala só o serviço / remove o serviço (administrador) |
| `clarim.exe devices` | lista as entradas e saídas de áudio |
| `clarim.exe sessions` | lista os programas que estão tocando áudio, sem alterar nada (`[in call...]` marca o programa da Mídia que está usando um microfone agora e tocaria no Chat) |
| `clarim.exe selftest` | mede o custo do processamento com áudio sintético, sem tocar em dispositivos |
| `clarim.exe reset-routing --yes` | apaga todas as rotas por programa (sem `--yes` só mostra o aviso) |

## Problemas comuns

- **Log**: `%LOCALAPPDATA%\Clarim\clarim.log` (volta a zero ao passar de 5 MB). Veja aqui primeiro. O serviço grava o dele no `%LOCALAPPDATA%` da conta de sistema.
- **A instalação guiada falhou ou ficou algo pendente**: o resumo no fim diz o que fazer em cada item, e `C:\Program Files\Clarim\install.log` tem o registro de cada passo. Rodar `clarim.exe setup` de novo só refaz o que falta. O VB-Cable às vezes só termina de instalar depois de reiniciar o PC. Duas instalações guiadas ao mesmo tempo não rodam: a segunda avisa que já há uma em andamento.
- **`clarim.exe install` falhou no serviço**: veja `C:\Program Files\Clarim\install.log` (o erro completo está no fim; se o serviço antigo já tinha sido parado, o Clarim tenta iniciá-lo de novo, então o Clarim anterior continua funcionando). O `setup` nunca mexe num serviço que já está instalado (só o inicia, se estiver parado); se a primeira instalação do serviço falhar no meio, ele desfaz o que fez, e se nem isso for possível o resumo manda rodar `clarim.exe uninstall` (como administrador) e depois `clarim.exe setup`. O `clarim.exe uninstall` também grava ali e deixa o arquivo.
- **Configuração**: `%APPDATA%\Clarim\config.json`. Se o arquivo estiver corrompido, o Clarim o renomeia para `config.json.bad` e começa do padrão.
- **Painel não abre / porta ocupada**: o painel usa a porta **47800** (campo `port` do `config.json`). Se outro programa usa essa porta, o agente registra o erro no log e encerra sem ligar o microfone; o serviço tenta de novo sozinho, com espera crescente de até 30 s. Se já existe um agente rodando (por exemplo um aberto à mão), o novo sai com código 3 e o serviço tenta de novo a cada 30 s.
- **"Nenhum microfone virtual encontrado"**: rode `clarim.exe setup` (ou instale o VB-Cable em <https://vb-audio.com/Cable/>) e reinicie o Clarim; se acabou de instalar, reinicie o PC. Outro cabo virtual (nome com "Virtual Audio Driver" ou "Virtual Mic") também serve.
- **Microfone sem som**: o painel mostra o aviso amarelo "Sem sinal do microfone" quando o Windows entrega só silêncio por uns 2 s ou a entrada fica em silêncio total por uns 5 s (o ícone de status continua verde: o microfone está rodando). Confira se o microfone não está mudo e, em Configurações do Windows > Privacidade > Microfone, se os apps de área de trabalho podem usá-lo.
- **Minha voz chega baixa na chamada**: com o Volume automático e a chave **Ajustar o volume do microfone no Windows se estourar** ligados, o painel mostra "Sua voz está chegando baixa mesmo com o volume automático no máximo". Suba o volume do microfone no Windows ou aproxime o microfone. O Volume automático chega a +20 dB e não deixa o ruído entre as palavras passar de −55 dBFS, então num microfone com muito ruído de fundo o ganho máximo fica menor.
- **O volume da minha voz sobe e desce**: quase sempre é o controle automático de ganho do Discord/Meet/Zoom brigando com o Volume automático do Clarim. Desligue o do programa (veja "Usar o microfone virtual no Discord, Meet, Zoom").
- **O volume do microfone no Windows mudou sozinho**: foi o Clarim, porque o microfone estava estourando durante uma chamada (veja "Se o microfone estourar: volume do Windows"). Ele devolve quando a voz fica baixa e ao sair. Para ele não mexer mais, desligue "Ajustar o volume do microfone no Windows se estourar". Se o painel disser que o microfone satura antes do volume do Windows, ele não tem volume de hardware: diminua o "Aumento do microfone" (Propriedades do microfone > Níveis) ou afaste o microfone.
- **Apareceu aviso de vazamento, mas ninguém reclama**: o eco vem do som das caixas (jogo, vídeo, outra chamada), e o programa da chamada pode já estar cortando esse eco (veja os limites em "Avisos de vazamento na chamada"). Abaixe as caixas, use fone, ou desligue "Avisar se vazar som na chamada".
- **O aviso de vazamento não aparece**: ele só vale em chamada, então confira se o painel mostra "Em chamada: ..." (o programa precisa usar o **CABLE Output** como microfone; o OBS não conta, e o painel leva até uns 3 s para notar). O balão não aparece em tela cheia ou com o Windows ocupado, mas o aviso amarelo no painel sim. Eco muito alto, perto do volume da sua voz, não é medido. Durante a chamada o `clarim.log` mostra uma linha a cada 10 s com o que o Clarim está medindo (eco, atraso, nível do eco, nível da voz, ganho, ruído, referências).
- **"Ouvir meu microfone" não toca nada**: se a saída escolhida (fones) não for encontrada, ou parar depois (fones desligados), o painel mostra um aviso amarelo embaixo de "Fones" e o microfone virtual segue funcionando. O Clarim não tenta de novo sozinho, para não cortar o microfone no meio de uma chamada: com os fones de volta, desligue e ligue "Ouvir meu microfone".
- **"Instalar equalizador" está desligado**: o serviço do Clarim não está instalado ou não respondeu (o painel diz qual). Rode `clarim.exe install` como administrador; se o serviço é de uma versão antiga, rode de novo para atualizá-lo.
- **A instalação do equalizador falhou**: o painel mostra o motivo no aviso amarelo (sem internet, "hash diferente" = o arquivo baixado não é o instalador esperado e não foi executado, o instalador não terminou...). O serviço registra os detalhes no `clarim.log` da conta de sistema. Nada é deixado pela metade: o aparelho em que a alteração falhou volta ao que era, e a cópia original fica em `C:\Program Files\Clarim\backup`. Se a instalação foi interrompida e sobrou só parte dos arquivos (sem o registro no Windows), o painel trata o Equalizer APO como **não instalado** e volta a mostrar "Instalar equalizador", que roda o instalador de novo por cima.
- **Reiniciar o áudio falhou ou o som não voltou**: reinicie o PC. O painel diz quando for esse o caso. O Clarim tenta iniciar o Windows Audio de novo a cada 2 s por até 25 s e, mesmo que ele não volte, religa os serviços que dependem dele. Se você parar ou atualizar o serviço do Clarim no meio de um reinício, ele espera até 8 s para o reinício terminar.
- **O Forte não inicia**: o painel mostra "Em uso: Leve" e o log diz o motivo; o Clarim cai para o RNNoise sozinho.
- **Microfone caiu depois de desligar o USB**: o agente tenta de novo sozinho a cada poucos segundos.
- `clarim.exe devices` e `clarim.exe sessions` mostram o que o Clarim enxerga.

## Licença

MIT.
