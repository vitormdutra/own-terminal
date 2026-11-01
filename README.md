# own-terminal

Terminal leve em Rust. Emulação via `alacritty_terminal`, janela e desenho via `eframe`/`egui`.

## Instalar

1. Instale o Rust (uma vez só):

   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   ```

   Abra um terminal novo e confirme com `cargo --version`.

2. Compile e instale o binário:

   ```bash
   git clone <url-do-repo> own-terminal && cd own-terminal
   cargo install --path .
   ```

   O binário `own-terminal` vai para `~/.cargo/bin` (já no PATH depois do passo 1).

   Só para testar sem instalar: `cargo run --release`.

### Abrir sem precisar de outro terminal (macOS)

Gera um app de verdade, que você abre pelo Spotlight, Launchpad ou Dock:

```bash
./bundle.sh
cp -R dist/OwnTerminal.app /Applications/
```

Na primeira vez, se o macOS bloquear o app (não é assinado): clique com o botão direito em
`OwnTerminal` → **Abrir** → **Abrir**. Depois é só `Cmd+Espaço` → "OwnTerminal".
Para fixar no Dock, arraste o app para lá. Começa na sua pasta home.

## Usar

```bash
own-terminal
```

Abre uma janela com o seu shell padrão (`$SHELL`).

| Ação | Como |
|------|------|
| Nova aba | `Cmd+T` ou botão `+` (abre na pasta da divisão em foco; as divisões também) |
| Dividir lado a lado | `Cmd+D` |
| Dividir empilhado | `Cmd+Shift+D` (dá para misturar as duas direções) |
| Redimensionar divisão | Arrastar o divisor com o mouse |
| Mudar o foco entre divisões | `Cmd+Option+←↑↓→` (vai para a vizinha naquela direção), `Cmd+]` / `Cmd+[` (próxima/anterior) ou clicar. A divisão em foco tem uma moldura fina |
| Fechar divisão/aba | `Cmd+W` (fecha a divisão em foco; a aba some com a última divisão, e a janela com a última aba) |
| Ir para a aba N | `Cmd+1` … `Cmd+9` ou clicar na aba |
| Selecionar texto | Arrastar com o mouse; duplo clique = palavra; triplo clique = linha |
| Zoom da fonte | `Cmd+=` aumenta, `Cmd+-` diminui, `Cmd+0` volta ao tamanho do config |
| Buscar no histórico | `Cmd+F`; `Enter` = ocorrência anterior, `Shift+Enter` = próxima, `Esc` fecha |
| Limpar seleção | Clicar |
| Copiar | `Cmd+C` (macOS) / `Ctrl+C` com seleção em outros SOs |
| Colar | `Cmd+V` (macOS) / `Ctrl+V` |
| Abrir link | `Cmd+clique` numa URL `http(s)://` (o cursor vira mãozinha) |
| Rolar histórico | Roda do mouse / trackpad |
| Fechar | Sair do shell (`exit`, `Ctrl+D`) ou fechar a janela |

## Configuração

Opcional. Crie `~/.config/own-terminal/config` (reinicie o app para aplicar):

```
# comentários começam com #
font_size = 15
font_path = /Users/voce/Library/Fonts/JetBrainsMono-Regular.ttf
fg = #d8d8d8
bg = #181818
cursor = block        # block | bar | underline
cursor_blink = false  # true para piscar
notify_after = 10     # avisa quando um comando com mais de N segundos termina (0 desliga)
```

Sem arquivo, valem os padrões acima. `Cmd+,` recarrega o arquivo sem reiniciar (cores, fonte e tamanho valem na hora; a forma do cursor só vale para abas novas). Programas como o vim podem trocar a forma do cursor sozinhos. O título da janela segue o título do programa em foco. `font_path` aponta para um `.ttf`/`.otf` monoespaçado.

## Aviso de comando longo

Quando um comando que rodou por `notify_after` segundos (padrão 10) ou mais termina e a janela
está em segundo plano, o app mostra uma notificação e faz o ícone pular no Dock. Funciona
detectando qual processo está em primeiro plano no terminal, então não precisa configurar o shell.
No macOS a notificação aparece como vinda do "Script Editor"; se não aparecer, libere em
Ajustes do Sistema → Notificações.

## Atalhos de edição (estilo macOS)

| Atalho | Faz |
|--------|-----|
| `Cmd+Backspace` | apaga a linha até o cursor |
| `Cmd+←` / `Cmd+→` | início / fim da linha |
| `Option+Backspace` | apaga a palavra |
| `Option+←` / `Option+→` | pula uma palavra |

## Limitações atuais

- Divisões: não há como trocar de lugar nem "zoom" de uma divisão.
- Busca: só vê o histórico do terminal. Programas em tela cheia (nano, vim, less) não deixam histórico, então use a busca deles (`Ctrl+W` no nano, `/` no vim e no less).
- Busca: ignora maiúsculas, procura texto simples (sem regex) e não atualiza sozinha com saída nova; digite de novo para refazer.
- Aviso de comando longo: só macOS e Linux; assume que o terminal está ocioso 2s depois de aberto.
- Sem reordenar abas.
- Itálico, sublinhado e tachado são desenhados; negrito vira cor mais clara (não há fonte em negrito). Só `fg`, `bg`, fonte e tamanho são configuráveis (paleta ANSI fixa no código).
- No Linux/Windows compila em teoria, mas só foi testado no macOS.

## Atualizar

```bash
git pull && cargo install --path . && ./bundle.sh
```

## Desinstalar

```bash
cargo uninstall own-terminal
```
