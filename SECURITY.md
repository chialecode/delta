# DELTA 数据与安全说明

DELTA 处理个人金融数据。密钥通过 OS 凭据存储引用；导入文件、账本、笔记和模型上下文不进入公开日志或仓库。实现要求见 [数据与安全](docs/dev-rules/data-and-security.md)。

报告问题时提供受影响版本、合成复现、预期/实际及脱敏日志，不贴真实密钥或账户明细。敏感问题请通过 [GitHub 私密漏洞报告](https://github.com/chialecode/delta/security/advisories/new)提交，不要在公开 Discussions 中披露；当前没有公开安全邮箱或漏洞奖励渠道。

文档中的安全目标不代表已交付安全功能。全项目按 D-22 采用 100% 纯 Rust 实现，不启动外部脚本子进程；首版不运行任意第三方扩展或不可信代码。
