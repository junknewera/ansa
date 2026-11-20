{ pkgs, integration }:
let
  snacks = pkgs.vimPlugins.snacks-nvim.overrideAttrs (_: {
    version = "882c996";
    src = pkgs.fetchFromGitHub {
      owner = "folke"; repo = "snacks.nvim";
      rev = "882c996cf28183f4d63640de0b4c02ec886d01f2";
      hash = "sha256-gU0XjCcnmZNwZ/erukA8miBiMKaSsSiInLtiv+OyEJI=";
    };
  });
  markdown = pkgs.vimPlugins.render-markdown-nvim.overrideAttrs (_: {
    version = "8801162";
    src = pkgs.fetchFromGitHub {
      owner = "MeanderingProgrammer"; repo = "render-markdown.nvim";
      rev = "8801162e041e34c64d0d1397cd1f353a14b01950";
      hash = "sha256-kpA9eecmWt88+jZrb63t2dUuZRY3kxTRnqKfcl3JdXA=";
    };
  });
  parsers = pkgs.vimPlugins.nvim-treesitter.withPlugins (grammars: [ grammars.markdown grammars.markdown_inline ]);
  editor = pkgs.neovim.override {
  withPython3 = false;
  withNodeJs = false;
  withRuby = false;
  configure = {
    packages.ansa.start = [ snacks markdown parsers ];
    customRC = ''
      set hidden
      set mouse=a
      highlight Normal guibg=NONE ctermbg=NONE
      highlight NormalFloat guibg=NONE ctermbg=NONE
      lua << EOF
      vim.opt.runtimepath:prepend("${integration}")
      vim.opt.termguicolors = vim.env.NO_COLOR == nil
      require('ansa').setup({render=true,open_history=true,statusline=true})
      vim.schedule(function()
        local socket=vim.env.ANSA_WORKSPACE_SOCKET
        if socket and socket~="" then require('ansa').attach(socket,{editor=vim.env.ANSA_WORKSPACE_EDITOR=='1'}) end
      end)
      EOF
    '';
  };
  };
in pkgs.symlinkJoin {
  name = "ansa-workspace-neovim";
  paths = [ editor ];
  nativeBuildInputs = [ pkgs.makeWrapper ];
  postBuild = ''
    wrapProgram "$out/bin/nvim" --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.imagemagick ]}
  '';
}
