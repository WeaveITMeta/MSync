# DSync Bot Dashboard Export Specification

## 🎯 Project Overview

**DSync** is a Rust-based Discord bot that implements merit-based open source community management. It automatically synchronizes Discord roles based on GitHub contributions and code quality assessments from CodeRabbit AI.

### Core Purpose
- **Merit-Based Role Management**: Automatically assign Discord roles based on contribution quality
- **GitHub Integration**: Track pull requests, reviews, and contributions
- **Code Quality Assessment**: Integrate with CodeRabbit for automated code reviews
- **Community Engagement**: Leaderboards, statistics, and progress tracking

---

## 🏗️ Technical Architecture

### Backend Stack
- **Language**: Rust (Bot) + Elixir (Dashboard)
- **Bot Framework**: Actix Web + Serenity
- **Dashboard Framework**: Phoenix LiveView
- **Database**: Supabase PostgreSQL with Ecto
- **Real-time**: Phoenix Channels + Supabase Realtime
- **GitHub API**: Octocrab (Rust) + HTTPoison (Elixir)

### **Why Phoenix LiveView + Supabase?**
- **Native Real-time**: Phoenix LiveView + Supabase Realtime subscriptions
- **Minimal JavaScript**: LiveView handles real-time UI updates server-side
- **Fault Tolerance**: OTP supervision trees ensure reliability
- **Concurrent Users**: Elixir's Actor model handles thousands of concurrent connections
- **Live Components**: Reusable, stateful UI components
- **Managed Database**: Supabase provides managed PostgreSQL with built-in features
- **Real-time Database**: Supabase Realtime for database change subscriptions
- **Built-in Auth**: Supabase Auth for OAuth and user management
- **Auto-generated APIs**: Supabase automatically generates REST APIs

---

## 📁 Phoenix Application Structure
```
📁 dsync_dashboard/
├── 📁 lib/dsync_web/
│   ├── 📁 live/
│   │   ├── dashboard_live.ex          # Main admin dashboard
│   │   ├── analytics_live.ex          # Real-time analytics
│   │   ├── contributor_live.ex        # Contributor management
│   │   ├── leaderboard_live.ex        # Live leaderboards
│   │   └── configuration_live.ex      # Merit system config
│   ├── 📁 components/
│   │   ├── merit_display.ex           # Merit point components
│   │   ├── contribution_card.ex       # Contribution cards
│   │   ├── role_progress.ex           # Role progression bars
│   │   └── leaderboard_entry.ex       # Leaderboard components
│   ├── 📁 controllers/
│   │   ├── auth_controller.ex          # OAuth handling
│   │   └── api_controller.ex           # REST API endpoints
├── 📁 lib/dsync/
│   ├── analytics.ex                   # Analytics calculations
│   ├── bot_client.ex                  # Rust bot integration
│   ├── github_integration.ex          # GitHub webhook handling
│   ├── merit_calculator.ex            # Merit point logic
│   ├── accounts.ex                    # User management
│   └── real_time.ex                   # PubSub broadcasting
├── 📁 priv/repo/migrations/           # Database migrations
└── 📁 assets/                         # Minimal CSS/JS assets
```

---

## 📊 Database Schema (Supabase)

### Contributors Table
```sql
-- Supabase PostgreSQL with Row Level Security
CREATE TABLE contributors (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    github_username TEXT UNIQUE NOT NULL,
    discord_user_id TEXT UNIQUE,
    total_merit_points INTEGER DEFAULT 0,
    current_role TEXT DEFAULT 'Newcomer',
    contributions_count INTEGER DEFAULT 0,
    streak_days INTEGER DEFAULT 0,
    last_contribution_date TIMESTAMPTZ,
    created_at TIMESTAMPTZ DEFAULT NOW(),
    updated_at TIMESTAMPTZ DEFAULT NOW()
);

-- Enable Row Level Security
ALTER TABLE contributors ENABLE ROW LEVEL SECURITY;

-- Policy for authenticated users to read all contributors
CREATE POLICY "Contributors are viewable by authenticated users"
ON contributors FOR SELECT
TO authenticated
USING (true);

-- Policy for service role to manage contributors
CREATE POLICY "Service role can manage contributors"
ON contributors FOR ALL
USING (auth.role() = 'service_role');

-- Enable Realtime
ALTER PUBLICATION supabase_realtime ADD TABLE contributors;
```

### Contributions Table
```sql
CREATE TABLE contributions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    contributor_id UUID NOT NULL REFERENCES contributors(id) ON DELETE CASCADE,
    github_pr_number INTEGER NOT NULL,
    title TEXT NOT NULL,
    description TEXT,
    status TEXT DEFAULT 'open',
    merit_points INTEGER DEFAULT 0,
    lines_added INTEGER DEFAULT 0,
    lines_removed INTEGER DEFAULT 0,
    files_changed INTEGER DEFAULT 0,
    coderabbit_score REAL,
    created_at TIMESTAMPTZ DEFAULT NOW(),
    merged_at TIMESTAMPTZ
);

-- Enable Row Level Security
ALTER TABLE contributions ENABLE ROW LEVEL SECURITY;

-- Policy for authenticated users to view contributions
CREATE POLICY "Contributions are viewable by authenticated users"
ON contributions FOR SELECT
TO authenticated
USING (true);

-- Enable Realtime
ALTER PUBLICATION supabase_realtime ADD TABLE contributions;
```

### Merit Roles Table
```sql
CREATE TABLE merit_roles (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    role_name TEXT UNIQUE NOT NULL,
    min_merit_points INTEGER NOT NULL,
    discord_role_id TEXT,
    color_hex TEXT,
    description TEXT,
    created_at TIMESTAMPTZ DEFAULT NOW()
);

-- Enable Row Level Security
ALTER TABLE merit_roles ENABLE ROW LEVEL SECURITY;

-- Policy for all users to read roles
CREATE POLICY "Merit roles are viewable by everyone"
ON merit_roles FOR SELECT
USING (true);

-- Enable Realtime
ALTER PUBLICATION supabase_realtime ADD TABLE merit_roles;
```

### Merit Activities Table
```sql
CREATE TABLE merit_activities (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    contributor_id UUID NOT NULL REFERENCES contributors(id) ON DELETE CASCADE,
    activity_type TEXT NOT NULL,
    points_awarded INTEGER NOT NULL,
    description TEXT,
    github_pr_number INTEGER,
    created_at TIMESTAMPTZ DEFAULT NOW()
);

-- Enable Row Level Security
ALTER TABLE merit_activities ENABLE ROW LEVEL SECURITY;

-- Policy for authenticated users to view activities
CREATE POLICY "Merit activities are viewable by authenticated users"
ON merit_activities FOR SELECT
TO authenticated
USING (true);

-- Enable Realtime
ALTER PUBLICATION supabase_realtime ADD TABLE merit_activities;
```

---

## 🎛️ Dashboard Requirements

### 1. **Admin Dashboard (Phoenix LiveView)**

#### **Overview Page (`DashboardLive`)**
```elixir
defmodule DsyncWeb.DashboardLive do
  use DsyncWeb, :live_view

  def mount(_params, _session, socket) do
    if connected?(socket) do
      # Subscribe to Phoenix PubSub
      Phoenix.PubSub.subscribe(Dsync.PubSub, "dashboard_updates")
      
      # Subscribe to Supabase Realtime
      Dsync.Supabase.subscribe_to_contributors()
    end
    
    {:ok, assign_dashboard_stats(socket)}
  end
  
  def handle_info({:supabase_realtime, %{"eventType" => "UPDATE", "new" => contributor}}, socket) do
    # Real-time UI updates from Supabase database changes
    {:noreply, update_contributor_display(socket, contributor)}
  end
end
```

- **Real-time Statistics Cards**
  - Total Contributors: `COUNT(contributors)`
  - Active Contributors (last 30 days): Live updating
  - Total Merit Points Awarded: Real-time accumulation
  - Pull Requests This Month: Live GitHub webhook updates

- **Activity Timeline (Live Component)**
  ```elixir
  defmodule DsyncWeb.ActivityTimelineComponent do
    use DsyncWeb, :live_component
    
    def handle_info({:new_activity, activity}, socket) do
      activities = [activity | socket.assigns.activities] |> Enum.take(10)
      {:noreply, assign(socket, :activities, activities)}
    end
  end
  ```

- **Quick Actions**
  - Manual role sync button (LiveView form)
  - Bulk contributor import from GitHub
  - Merit point adjustment tools

#### **Contributors Management (`ContributorLive`)**
```elixir
defmodule DsyncWeb.ContributorLive do
  use DsyncWeb, :live_view
  
  def handle_event("adjust_merit", %{"points" => points, "contributor_id" => id}, socket) do
    case Merit.adjust_points(id, points) do
      {:ok, contributor} ->
        # Broadcast update to all connected dashboards
        Phoenix.PubSub.broadcast(Dsync.PubSub, "merit_updates", 
          {:merit_adjusted, contributor})
        {:noreply, socket}
      {:error, reason} ->
        {:noreply, put_flash(socket, :error, reason)}
    end
  end
end
```

- **Live Contributor Table**
  - Real-time updates as merit points change
  - Instant role progression notifications
  - Live search and filtering

- **Individual Contributor View (Live Component)**
  - Real-time profile updates
  - Live contribution feed
  - Instant merit adjustments with animations

#### **Merit System Configuration (`ConfigurationLive`)**
```elixir
defmodule DsyncWeb.ConfigurationLive do
  use DsyncWeb, :live_view
  
  def handle_event("update_role_threshold", params, socket) do
    case MeritRoles.update_threshold(params) do
      {:ok, role} ->
        # Broadcast to trigger role recalculations
        Phoenix.PubSub.broadcast(Dsync.PubSub, "role_updates", 
          {:threshold_updated, role})
        {:noreply, socket}
    end
  end
end
```

### 2. **Analytics Dashboard (`AnalyticsLive`)**

#### **Real-time Charts and Graphs**
```elixir
defmodule DsyncWeb.AnalyticsLive do
  use DsyncWeb, :live_view
  
  def mount(_params, _session, socket) do
    if connected?(socket) do
      # Subscribe to Phoenix PubSub
      Phoenix.PubSub.subscribe(Dsync.PubSub, "contributions")
      
      # Subscribe to Supabase Realtime
      Dsync.Supabase.subscribe_to_contributions()
    end
    
    {:ok, assign_analytics_data(socket)}
  end
  
  def handle_info({:supabase_realtime, %{"eventType" => "INSERT", "new" => contribution}}, socket) do
    # Update charts in real-time from Supabase
    updated_data = Analytics.recalculate_charts(contribution)
    {:noreply, push_event(socket, "update_charts", updated_data)}
  end
end
```

- **Live Charts** (using Chart.js with Phoenix hooks)
  - Contributions over time (updating as webhooks arrive)
  - Merit points distribution (live histogram)
  - Top contributors by category (real-time bar chart)
  - Role distribution pie chart (live updates)

#### **Live Leaderboard Views**
```elixir
defmodule DsyncWeb.LeaderboardLive do
  use DsyncWeb, :live_view
  
  def handle_info({:leaderboard_updated, period}, socket) do
    if socket.assigns.current_period == period do
      leaderboard = Analytics.get_leaderboard(period)
      {:noreply, assign(socket, :leaderboard, leaderboard)}
    else
      {:noreply, socket}
    end
  end
end
```

### 3. **User Portal** (For Contributors)

#### **Personal Dashboard (`ProfileLive`)**
```elixir
defmodule DsyncWeb.ProfileLive do
  use DsyncWeb, :live_view
  
  def mount(%{"id" => contributor_id}, _session, socket) do
    if connected?(socket) do
      # Subscribe to Phoenix PubSub
      Phoenix.PubSub.subscribe(Dsync.PubSub, "contributor:#{contributor_id}")
      
      # Subscribe to Supabase Realtime
      Dsync.Supabase.subscribe_to_contributor(contributor_id)
    end
    
    {:ok, assign_profile_data(socket, contributor_id)}
  end
  
  def handle_info({:merit_awarded, points, reason}, socket) do
    # Live merit point animations
    {:noreply, 
     socket
     |> update_merit_display(points)
     |> push_event("animate_merit_gain", %{points: points, reason: reason})}
  end
end
```

---

## 🔌 Phoenix Integration with Rust Bot

### **Bot Client Module**
```elixir
defmodule Dsync.BotClient do
  @bot_api_url Application.get_env(:dsync, :bot_api_url)
  
  def sync_roles(contributor_id) do
    HTTPoison.post("#{@bot_api_url}/api/sync-roles", 
      Jason.encode!(%{contributor_id: contributor_id}),
      [{"Content-Type", "application/json"}])
  end
  
  def get_bot_status do
    case HTTPoison.get("#{@bot_api_url}/health") do
      {:ok, %{status_code: 200}} -> :healthy
      _ -> :unhealthy
    end
  end
  
  def get_contributor_stats(contributor_id) do
    HTTPoison.get("#{@bot_api_url}/api/contributors/#{contributor_id}")
  end
end
```

### **Real-time Integration**
```elixir
defmodule Dsync.RealTime do
  def broadcast_merit_update(contributor) do
    Phoenix.PubSub.broadcast(Dsync.PubSub, "merit_updates", 
      {:merit_updated, contributor})
    
    Phoenix.PubSub.broadcast(Dsync.PubSub, "contributor:#{contributor.id}", 
      {:merit_awarded, contributor.total_merit_points, "GitHub contribution"})
  end
  
  def broadcast_role_promotion(contributor, old_role, new_role) do
    Phoenix.PubSub.broadcast(Dsync.PubSub, "role_updates", 
      {:role_promoted, contributor, old_role, new_role})
  end
end
```

### **GitHub Webhook Handler**
```elixir
defmodule Dsync.GitHubWebhook do
  def handle_webhook(payload) do
    case process_contribution(payload) do
      {:ok, contribution} ->
        # Update merit points
        {:ok, contributor} = Merit.award_points(contribution)
        
        # Broadcast to all live dashboards
        Dsync.RealTime.broadcast_merit_update(contributor)
        
        # Check for role promotion
        check_role_promotion(contributor)
        
      {:error, reason} -> 
        Logger.error("Webhook processing failed: #{reason}")
    end
  end
end
```

---

## 🎨 UI/UX Specifications

### **Design System**
- **Color Scheme**: Dark theme with merit-based color coding
  - Primary: `#7289DA` (Discord Blurple)
  - Success: `#43B581` (Green for positive actions)
  - Warning: `#FAA61A` (Orange for pending items)
  - Error: `#F04747` (Red for errors)
  - Merit Levels: Gradient from blue → purple → gold

### **Phoenix LiveView Components**

#### **Merit Point Display Component**
```elixir
defmodule DsyncWeb.MeritDisplayComponent do
  use DsyncWeb, :live_component

  def render(assigns) do
    ~H"""
    <div class="merit-display" phx-hook="AnimateMerit" id={"merit-#{@contributor.id}"}>
      <div class="current-points">
        <%= @contributor.total_merit_points %>
      </div>
      <div class="progress-container">
        <div class="progress-bar">
          <div class="progress-fill" style={"width: #{@progress_percentage}%"}></div>
        </div>
        <div class="role-info">
          <span class="current-role"><%= @contributor.current_role %></span>
          <span class="next-role">→ <%= @next_role.name %></span>
        </div>
      </div>
      <div class="points-to-next">
        <%= @next_role.min_merit_points - @contributor.total_merit_points %> points to next role
      </div>
    </div>
    """
  end
end
```

#### **Live Contribution Card**
```elixir
defmodule DsyncWeb.ContributionCardComponent do
  use DsyncWeb, :live_component

  def render(assigns) do
    ~H"""
    <div class="contribution-card" id={"contribution-#{@contribution.id}"}>
      <div class="contribution-header">
        <h3><%= @contribution.title %></h3>
        <span class="pr-number">#<%= @contribution.github_pr_number %></span>
      </div>
      
      <div class="contribution-stats">
        <div class="merit-points">
          <span class="points"><%= @contribution.merit_points %></span>
          <span class="label">Merit Points</span>
        </div>
        
        <%= if @contribution.coderabbit_score do %>
          <div class="quality-score">
            <span class="score"><%= @contribution.coderabbit_score %></span>
            <span class="label">Quality Score</span>
          </div>
        <% end %>
        
        <div class="lines-changed">
          <span class="added">+<%= @contribution.lines_added %></span>
          <span class="removed">-<%= @contribution.lines_removed %></span>
        </div>
      </div>
      
      <div class="contribution-status">
        <span class={"status status-#{@contribution.status}"}><%= @contribution.status %></span>
        <time datetime={@contribution.created_at}>
          <%= relative_time(@contribution.created_at) %>
        </time>
      </div>
    </div>
    """
  end
end
```

---

## 🔐 Security & Permissions

### **Phoenix Authentication**
```elixir
defmodule DsyncWeb.AuthController do
  use DsyncWeb, :controller
  
  def discord_callback(conn, %{"code" => code}) do
    with {:ok, discord_token} <- Discord.OAuth.exchange_code(code),
         {:ok, discord_user} <- Discord.API.get_user(discord_token),
         {:ok, supabase_user} <- create_supabase_user(discord_user) do
      
      conn
      |> put_session(:supabase_token, supabase_user.access_token)
      |> put_session(:user_id, supabase_user.id)
      |> redirect(to: "/dashboard")
    end
  end
  
  defp create_supabase_user(discord_user) do
    # Create or update user in Supabase Auth
    Supabase.auth()
    |> Supabase.sign_up(%{
      email: "#{discord_user.username}@discord.local",
      password: generate_secure_password(),
      user_metadata: %{
        discord_id: discord_user.id,
        discord_username: discord_user.username,
        avatar_url: discord_user.avatar
      }
    })
  end
end
```

### **Live View Authentication**
```elixir
defmodule DsyncWeb.AuthLive do
  import Phoenix.LiveView
  
  def on_mount(:ensure_authenticated, _params, session, socket) do
    case session["user_id"] do
      nil -> {:halt, redirect(socket, to: "/auth/discord")}
      user_id -> 
        user = Accounts.get_user!(user_id)
        {:cont, assign(socket, :current_user, user)}
    end
  end
end
```

---

## 🚀 Deployment Configuration

### **Elixir/Phoenix + Supabase Environment Variables**
```env
# Phoenix Configuration
SECRET_KEY_BASE=your-very-long-secret-key
PHX_HOST=your-dashboard-domain.com
PORT=4000

# Supabase Configuration
SUPABASE_URL=https://your-project.supabase.co
SUPABASE_ANON_KEY=your-supabase-anon-key
SUPABASE_SERVICE_KEY=your-supabase-service-role-key
SUPABASE_JWT_SECRET=your-supabase-jwt-secret

# Database (Supabase PostgreSQL)
DATABASE_URL=postgresql://postgres:[YOUR-PASSWORD]@db.your-project.supabase.co:5432/postgres

# DSync Bot Integration
DSYNC_BOT_API_URL=https://your-bot-domain.com/api
DSYNC_BOT_WEBSOCKET_URL=wss://your-bot-domain.com/ws

# OAuth Configuration (can use Supabase Auth providers)
DISCORD_CLIENT_ID=your-discord-client-id
DISCORD_CLIENT_SECRET=your-discord-client-secret
GITHUB_CLIENT_ID=your-github-client-id
GITHUB_CLIENT_SECRET=your-github-client-secret

# External APIs
CODERABBIT_API_KEY=your-coderabbit-api-key

# Security
GUARDIAN_SECRET_KEY=your-jwt-secret
CORS_ORIGINS=https://your-dashboard-domain.com
```

### **Phoenix + Supabase Production Setup**
```elixir
# config/prod.exs
config :dsync, DsyncWeb.Endpoint,
  url: [host: System.get_env("PHX_HOST"), port: 443, scheme: "https"],
  check_origin: [
    "https://your-dashboard-domain.com",
    "https://your-bot-domain.com",
    "https://your-project.supabase.co"
  ],
  cache_static_manifest: "priv/static/cache_manifest.json"

# Enable real-time features
config :dsync, DsyncWeb.Endpoint,
  live_view: [signing_salt: System.get_env("LIVE_VIEW_SALT")]

# PubSub for real-time updates
config :dsync, Dsync.PubSub,
  name: Dsync.PubSub,
  adapter: Phoenix.PubSub.PG2

# Supabase configuration
config :dsync, :supabase,
  url: System.get_env("SUPABASE_URL"),
  anon_key: System.get_env("SUPABASE_ANON_KEY"),
  service_key: System.get_env("SUPABASE_SERVICE_KEY"),
  jwt_secret: System.get_env("SUPABASE_JWT_SECRET")

# Database configuration for Supabase
config :dsync, Dsync.Repo,
  url: System.get_env("DATABASE_URL"),
  pool_size: 10,
  tls: true,
  tls_opts: [
    verify: :verify_peer,
    cacerts: :public_key.cacerts_get(),
    server_name_indication: :disable,
    customize_hostname_check: [
      match_fun: :public_key.pkix_verify_hostname_match_fun(:https)
    ]
  ]
```

---

## 📋 Development Checklist

### **Phase 1: Phoenix + Supabase Core Setup**
- [ ] Phoenix application with LiveView
- [ ] Supabase project setup and database schema
- [ ] Supabase Auth integration (Discord & GitHub OAuth)
- [ ] Basic contributor management (LiveView + Supabase)
- [ ] Real-time merit point display with Supabase Realtime
- [ ] Bot API client integration

### **Phase 2: Real-time Analytics with Supabase**
- [ ] Live analytics dashboard with Supabase queries
- [ ] Real-time leaderboard with Supabase Realtime subscriptions
- [ ] Supabase Edge Functions for GitHub webhook processing
- [ ] Live contribution tracking with database subscriptions

### **Phase 3: Advanced LiveView + Supabase Features**
- [ ] Live Components with Supabase real-time data
- [ ] Real-time notifications using Supabase + Phoenix
- [ ] Mobile-responsive LiveView templates
- [ ] Advanced filtering with Supabase PostgREST queries

### **Phase 4: Production Polish**
- [ ] Supabase Row Level Security policies
- [ ] Phoenix + Supabase deployment optimization
- [ ] Real-time performance monitoring
- [ ] Supabase Auth + Phoenix Guardian integration
- [ ] Load testing with Supabase connection pooling

---

## 🎯 Success Metrics

### **Dashboard KPIs**
- **Real-time Performance**: Sub-100ms LiveView + Supabase updates
- **Concurrent Users**: Support 1000+ concurrent dashboard users
- **Database Performance**: Supabase connection pooling efficiency
- **Feature Adoption**: Usage of live merit adjustment tools
- **Mobile Usage**: LiveView responsive design adoption

### **Phoenix + Supabase Metrics**
- **LiveView Connections**: Active real-time connections
- **Supabase Realtime**: Database subscription performance
- **PubSub Messages**: Real-time message throughput
- **Memory Usage**: Elixir process memory efficiency
- **Response Times**: Phoenix + Supabase query response times
- **Database Health**: Supabase connection pool status

---

## 🔌 API Endpoints

### **Current API Endpoints (Rust Bot)**
```
GET  /health                    - Health check
GET  /api/stats                 - Global statistics
GET  /api/leaderboard          - Top contributors
GET  /api/contributors         - List all contributors
GET  /api/contributors/:id      - Get specific contributor
POST /api/contributors/:id/sync - Manual role sync for contributor
POST /github/webhook           - GitHub webhook handler
POST /api/sync-roles           - Manual role synchronization
PUT  /api/contributors/:id      - Update contributor info
```

### **Required New API Endpoints (For Phoenix Dashboard)**
```
# Authentication & Authorization
POST /api/auth/bot-token        - Get bot API token for dashboard
GET  /api/auth/verify           - Verify dashboard authentication

# Merit System Management
POST /api/contributors/:id/merit    - Adjust merit points
PUT  /api/merit-roles              - Update merit role configuration
GET  /api/merit-rules              - Get point calculation rules

# Analytics & Reporting
GET  /api/analytics/overview        - Dashboard overview stats
GET  /api/analytics/contributors    - Contributor analytics
GET  /api/analytics/contributions   - Contribution analytics
GET  /api/analytics/leaderboard/:period - Leaderboard data

# Configuration Management
GET  /api/config/bot-status        - Bot connection and health status
GET  /api/config/webhooks          - GitHub webhook status
POST /api/config/roles-sync        - Trigger bulk role synchronization
