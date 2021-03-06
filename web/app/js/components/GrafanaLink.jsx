import React from 'react';

export default class GrafanaLink extends React.Component {
  render() {
    let ownerInfo = this.props.name.split("/");
    let namespace = ownerInfo[0];
    let name = ownerInfo[1];
    return (
      <this.props.conduitLink
        to={`/dashboard/db/conduit-${this.props.resource}?var-namespace=${namespace}&var-${this.props.resource}=${name}`}
        deployment={"grafana"}
        targetBlank={true}>
        {this.props.name}&nbsp;&nbsp;<i className="fa fa-external-link" />
      </this.props.conduitLink>
    );
  }
}
