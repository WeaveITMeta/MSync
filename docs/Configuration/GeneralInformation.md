# Discord Bot Setup Guide

This guide walks you through setting up your Discord application for the MSync (MeritSync) bot.

## 🔧 Discord Application Configuration

### Step 1: Create Discord Application

1. Go to [Discord Developer Portal](https://discord.com/developers/applications)
2. Click **"New Application"**
3. Enter your application name (e.g., "MSync Bot")
4. Click **"Create"**

### Step 2: General Information Setup

#### **Required Fields**

| Field | Value | Description |
|-------|--------|-------------|
| **Application ID** | `Copy this value` | Used as `DISCORD_APPLICATION_ID` in `.env` |
| **Public Key** | `Auto-generated` | Used for webhook verification |

#### **Optional Fields (for Production)**

| Field | Recommended Value | When to Use |
|-------|------------------|-------------|
| **Interactions Endpoint URL** | `https://your-domain.com/api/interactions` | When using HTTP interactions instead of Gateway |
| **Linked Roles Verification URL** | `https://your-domain.com/verify-user` | For Discord's Linked Roles feature |
| **Terms of Service URL** | `https://your-domain.com/terms-of-service` | Required for public bots |
| **Privacy Policy URL** | `https://your-domain.com/privacy-policy` | Required for public bots |

> 💡 **For Development**: Leave optional fields blank - they're not required for testing!

### Step 3: Bot Configuration

1. Navigate to the **"Bot"** section in the left sidebar
2. Click **"Add Bot"** (if not already created)
3. Configure the following settings:

#### **Bot Settings**

| Setting | Recommended Value | Purpose |
|---------|------------------|---------|
| **Username** | `MSync` | Your bot's display name |
| **Avatar** | Upload a logo | Visual identification |
| **Public Bot** | `Disabled` | Keep private during development |

#### **Privileged Gateway Intents**

Enable these intents for MSync to function properly:

- ✅ **Server Members Intent** - Required for role management
- ✅ **Message Content Intent** - Required for message handling
- ❌ **Presence Intent** - Not needed for MSync

#### **Bot Token**

1. Click **"Reset Token"** or **"Copy"** if first time
2. Copy the token value
3. Add to your `.env` file as `DISCORD_TOKEN`

⚠️ **Security Warning**: Never share your bot token publicly!

### Step 4: OAuth2 Configuration

1. Go to **"OAuth2"** → **"URL Generator"**
2. Select scopes:
   - ✅ `bot`
   - ✅ `applications.commands`

3. Select bot permissions:
   - ✅ **Manage Roles**
   - ✅ **Send Messages**
   - ✅ **Use Slash Commands**
   - ✅ **Manage Webhooks**
   - ✅ **Read Message History**
   - ✅ **Add Reactions**

4. Copy the generated URL to invite your bot to servers

### Step 5: Environment Configuration

Update your `.env` file with the Discord values:

```env
# Discord Configuration
DISCORD_TOKEN=your_bot_token_here
DISCORD_APPLICATION_ID=your_application_id_here
```

## 🚀 Quick Setup Checklist

- [ ] Created Discord application
- [ ] Copied Application ID to `.env`
- [ ] Created bot and copied token to `.env`
- [ ] Enabled required intents (Server Members, Message Content)
- [ ] Generated invite URL with proper permissions
- [ ] Invited bot to test server

## 🔍 Troubleshooting

### Common Issues

| Problem | Solution |
|---------|----------|
| Bot not responding to commands | Check if intents are enabled |
| Permission denied errors | Verify bot has required permissions in server |
| Token invalid | Regenerate token in Developer Portal |
| Commands not appearing | Ensure bot has `applications.commands` scope |

### Testing Your Setup

1. **Start the bot**:
   ```bash
   cargo run
   ```

2. **Test slash commands** in Discord:
   - `/link` - Link your GitHub account
   - `/stats` - View your contribution stats
   - `/leaderboard` - See top contributors

3. **Check logs** for any errors:
   ```bash
   # Enable debug logging
   RUST_LOG=debug cargo run
   ```

## 📚 Additional Resources

- [Discord Developer Documentation](https://discord.com/developers/docs)
- [Serenity Framework Guide](https://docs.rs/serenity/)
- [Discord Bot Best Practices](https://discord.com/developers/docs/topics/community-resources)

---

🎉 **Your Discord bot is now ready for MSync integration!**