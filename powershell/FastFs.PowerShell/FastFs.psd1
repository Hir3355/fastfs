@{
    RootModule = 'FastFs.PowerShell.dll'
    ModuleVersion = '0.6.2'
    GUID = 'd44bd8c8-2398-4bf4-9f57-aaf3d850bb72'
    Author = 'FastFs contributors'
    CompanyName = 'Community'
    Copyright = '(c) FastFs contributors. MIT License.'
    Description = 'Rust 製の高速ファイルシステム操作を PowerShell から直接呼び出します。'
    PowerShellVersion = '7.6'
    CompatiblePSEditions = @('Core')
    CmdletsToExport = @('Invoke-FastFs', 'Select-FastFsHead')
    FunctionsToExport = @()
    AliasesToExport = @('fastfs', 'head')
    VariablesToExport = @()
    PrivateData = @{
        PSData = @{
            Tags = @('filesystem', 'search', 'regex', 'rust', 'windows')
        }
    }
}
